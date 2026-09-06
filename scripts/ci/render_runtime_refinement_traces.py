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
DOCUMENT_KEYS = {
    "schema_version",
    "projection_id",
    "projected_fields",
    "name",
    "calls",
    "agent_calls",
    "max_attempts",
    "states",
    "transitions",
}
TRANSITION_KEYS = {"id", "from_version", "to_version"}


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


def transition_ids_from_model(model_path: Path) -> set[str]:
    model = model_path.read_text(encoding="utf-8")
    declaration = re.search(
        r"(?ms)^TransitionIds\s*==\s*\{(?P<body>.*?)\}", model
    )
    if declaration is None:
        raise ValueError(f"TransitionIds declaration missing from {model_path}")
    return {
        json.loads(literal)
        for literal in re.findall(r'"(?:\\.|[^"\\])*"', declaration.group("body"))
    }


def validate_document(document: dict[str, Any], manifest: dict[str, Any]) -> None:
    if set(document) != DOCUMENT_KEYS:
        raise ValueError(f"unexpected trace-document keys: {set(document) ^ DOCUMENT_KEYS}")
    if int(document["schema_version"]) != int(manifest["trace_schema_version"]):
        raise ValueError("trace schema version does not match the refinement manifest")
    if document["projection_id"] != manifest["projection_id"]:
        raise ValueError("trace projection ID does not match the refinement manifest")
    manifest_fields = [field["name"] for field in manifest["projected_fields"]]
    if document["projected_fields"] != manifest_fields:
        raise ValueError(
            "projected fields drifted from formal/runtime-refinement-manifest.json"
        )
    if set(manifest_fields) != STATE_KEYS:
        raise ValueError("manifest projected fields do not match the renderer schema")

    states = document["states"]
    transitions = document["transitions"]
    if len(transitions) != len(states) - 1:
        raise ValueError("a trace needs exactly one transition record per state pair")
    allowed_ids = {item["id"] for item in manifest["transition_families"]}
    for index, transition in enumerate(transitions):
        if set(transition) != TRANSITION_KEYS:
            raise ValueError(
                f"unexpected transition keys at index {index}: "
                f"{set(transition) ^ TRANSITION_KEYS}"
            )
        if transition["id"] not in allowed_ids:
            raise ValueError(f"unknown transition ID: {transition['id']}")
        if int(transition["from_version"]) != index:
            raise ValueError(f"transition {index} has a non-contiguous from_version")
        if int(transition["to_version"]) != index + 1:
            raise ValueError(f"transition {index} has a non-contiguous to_version")


def render(
    document: dict[str, Any], manifest: dict[str, Any], output: Path
) -> tuple[Path, Path]:
    validate_document(document, manifest)
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
    rendered_transition_ids = ", ".join(
        tla_string(str(transition["id"])) for transition in document["transitions"]
    )
    module_terminator = "=" * 77
    tla = f"""-------------------------- MODULE {module} --------------------------
EXTENDS ThreadCommitProjection

Trace == <<
    {rendered_states}
>>

ObservedTransitionIds == <<{rendered_transition_ids}>>

VARIABLE checker
TraceInit == checker = 0 /\\ state = InitialState
TraceNext == checker' = checker /\\ UNCHANGED state
TraceSpec == TraceInit /\\ [][TraceNext]_<<checker, state>>
TraceRefines == TraceIsRefinement(Trace)
TraceIdsRefine == TraceTransitionIdsRefine(Trace, ObservedTransitionIds)

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
INVARIANT TraceIdsRefine

CHECK_DEADLOCK FALSE
"""

    output.mkdir(parents=True, exist_ok=True)
    module_path = output / f"{module}.tla"
    config_path = output / f"{module}.cfg"
    module_path.write_text(tla, encoding="utf-8")
    config_path.write_text(cfg, encoding="utf-8")
    return module_path, config_path


def observation_value(value: Any, *, as_set: bool = False) -> str:
    if value is None:
        return "UnknownTime"
    if isinstance(value, bool):
        return "TRUE" if value else "FALSE"
    if isinstance(value, int) and value >= 0:
        return str(value)
    if isinstance(value, str):
        return tla_string(value)
    if isinstance(value, list):
        body = ", ".join(observation_value(item) for item in value)
        return "{" + body + "}" if as_set else "<<" + body + ">>"
    if isinstance(value, dict) and value and all(re.fullmatch(r"[a-z][a-z_]*", key) for key in value):
        return "[" + ", ".join(f"{key} |-> {observation_value(item)}" for key, item in value.items()) + "]"
    raise ValueError(f"unsupported observation value: {value!r}")


def normalize_observation_times(steps: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Order-embed observed times into TLC integers; retain legacy unknowns.

    The canonical observation relations use time only through equality, order
    and max (no duration/arithmetic). One injective, increasing map across the
    entire scenario therefore preserves AND reflects their truth, including
    deliberately corrupted timestamps. Revision/cursor fields are unchanged.
    """
    time_keys = {"now", "created", "updated", "time"}
    times: set[int] = set()

    def visit(value: Any, mapping: dict[int, int] | None = None, key: str = "") -> Any:
        if key in time_keys:
            if value is None:
                return None
            if type(value) is not int or value < 0:
                raise ValueError(f"invalid observed time: {value!r}")
            times.add(value)
            return value if mapping is None else mapping[value]
        if isinstance(value, dict):
            return {field: visit(item, mapping, field) for field, item in value.items()}
        if isinstance(value, list):
            return [visit(item, mapping) for item in value]
        return value

    visit(steps)
    return visit(steps, {time: rank for rank, time in enumerate(sorted(times), 1)})


def render_observations(trace_dir: Path, output: Path, repository: Path, manifest: dict[str, Any], require_complete: bool) -> int:
    profile = manifest["session_observations"]
    output.mkdir(parents=True, exist_ok=True)
    seen: set[str] = set()
    families: set[str] = set()
    for path in sorted(trace_dir.glob("*.json")):
        document = json.loads(path.read_text())
        if set(document) != {"schema_version", "projection_id", "name", "family", "steps"}:
            raise ValueError(f"invalid observation document keys: {path}")
        if document["schema_version"] != profile["schema_version"] or document["projection_id"] != profile["projection_id"]:
            raise ValueError(f"observation schema/identity drift: {path}")
        name, family = document["name"], document["family"]
        if name in seen or name != path.stem:
            raise ValueError(f"duplicate/mismatched observation scenario: {name}")
        scenario = profile["required_scenarios"].get(name)
        if scenario is None or scenario["family"] != family:
            raise ValueError(f"observation scenario/family drift: {name}")
        if not (repository / scenario["source"]).is_file():
            raise ValueError(f"observation producer missing: {scenario['source']}")
        definition = profile["families"][family]
        arguments = definition["arguments"]
        steps = document["steps"]
        if not steps:
            raise ValueError(f"empty observation scenario: {name}")
        expressions = []
        for step in normalize_observation_times(steps):
            if set(step) != set(arguments):
                raise ValueError(f"observation fields drift in {name}")
            values = [observation_value(step[key], as_set=family == "publication") for key in arguments]
            expressions.append(definition["operator"] + "(" + ", ".join(values) + ")")
        model = definition["model"]
        for dependency in [model, *definition["dependencies"]]:
            shutil.copyfile(repository / f"formal/tla/{dependency}.tla", output / f"{dependency}.tla")
        module = module_name(name)
        conjunction = "\n    /\\ ".join(expressions)
        (output / f"{module}.tla").write_text(
            f"---- MODULE {module} ----\nEXTENDS {model}\n"
            "TraceInit == Init\nTraceNext == UNCHANGED vars\nTraceSpec == TraceInit /\\ [][TraceNext]_vars\n"
            f"TraceRefines == /\\ {conjunction}\n" + "=" * 77 + "\n")
        config = (repository / f"formal/tla/{definition['config']}.cfg").read_text()
        constants = re.search(r"(?ms)^CONSTANTS?\b(.*?)(?=^(?:SPECIFICATION|INVARIANTS?|PROPERTY|PROPERTIES|CONSTRAINT|CHECK_DEADLOCK)\b|\Z)", config)
        if constants is None:
            raise ValueError(f"missing model constants: {model}")
        (output / f"{module}.cfg").write_text("CONSTANTS" + constants[1] + "\nSPECIFICATION TraceSpec\nINVARIANT TraceRefines\nCHECK_DEADLOCK FALSE\n")
        seen.add(name)
        families.add(family)
    missing = sorted(set(profile["required_scenarios"]) - seen)
    missing_families = sorted(set(profile["families"]) - families)
    report = {"projection_id": profile["projection_id"], "scenarios": sorted(seen), "missing": missing, "missing_families": missing_families, "complete": not missing and not missing_families}
    (output / "session-observation-coverage.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"session observation coverage: {len(seen)} scenarios, missing={missing}, missing_families={missing_families}")
    if not seen or require_complete and (missing or missing_families):
        raise ValueError("production observation trace coverage is incomplete")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--projection", choices=["thread-commit", "session-observations"], default="thread-commit")
    parser.add_argument("trace_dir", type=Path)
    parser.add_argument("output_dir", type=Path)
    parser.add_argument(
        "--manifest",
        type=Path,
        help="projection and transition-family manifest (defaults to repository manifest)",
    )
    parser.add_argument(
        "--coverage-report",
        type=Path,
        help="write the machine-readable transition coverage report at this path",
    )
    parser.add_argument(
        "--require-complete-transition-coverage",
        action="store_true",
        help="fail when any modeled transition family has no production trace hit",
    )
    args = parser.parse_args()

    if args.projection == "session-observations":
        repository = Path(__file__).resolve().parents[2]
        manifest = json.loads((args.manifest or repository / "formal/runtime-refinement-manifest.json").read_text())
        return render_observations(args.trace_dir, args.output_dir, repository, manifest, args.require_complete_transition_coverage)

    traces = sorted(args.trace_dir.glob("*.json"))
    if not traces:
        raise SystemExit(f"no ThreadCommit refinement traces in {args.trace_dir}")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    repository = Path(__file__).resolve().parents[2]
    manifest_path = args.manifest or (
        repository / "formal/runtime-refinement-manifest.json"
    )
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    transition_ids = [item["id"] for item in manifest["transition_families"]]
    if len(transition_ids) != len(set(transition_ids)):
        raise ValueError("duplicate transition family ID in refinement manifest")
    observations: dict[str, list[dict[str, Any]]] = {
        transition_id: [] for transition_id in transition_ids
    }
    model_path = repository / "formal/tla/ThreadCommitProjection.tla"
    model_transition_ids = transition_ids_from_model(model_path)
    if set(transition_ids) != model_transition_ids:
        raise ValueError(
            "transition families drifted between the manifest and ThreadCommitProjection: "
            f"manifest_only={set(transition_ids) - model_transition_ids}, "
            f"model_only={model_transition_ids - set(transition_ids)}"
        )
    shutil.copyfile(
        model_path,
        args.output_dir / "ThreadCommitProjection.tla",
    )
    for trace in traces:
        document = json.loads(trace.read_text(encoding="utf-8"))
        module, config = render(document, manifest, args.output_dir)
        for transition in document["transitions"]:
            observations[transition["id"]].append(
                {
                    "trace": document["name"],
                    "from_version": int(transition["from_version"]),
                    "to_version": int(transition["to_version"]),
                }
            )
        print(f"{module}\t{config}")

    covered = [transition_id for transition_id in transition_ids if observations[transition_id]]
    missing = [transition_id for transition_id in transition_ids if not observations[transition_id]]
    report = {
        "schema_version": 1,
        "projection_id": manifest["projection_id"],
        "trace_count": len(traces),
        "modeled_transition_count": len(transition_ids),
        "covered_transition_count": len(covered),
        "complete": not missing,
        "covered": covered,
        "missing": missing,
        "observations": observations,
    }
    report_path = args.coverage_report or (
        args.output_dir / "runtime-transition-coverage.json"
    )
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(
        f"runtime transition coverage: {len(covered)}/{len(transition_ids)}; "
        f"missing={','.join(missing) if missing else 'none'}"
    )
    print(f"runtime transition coverage report: {report_path}")
    if args.require_complete_transition_coverage and missing:
        raise SystemExit(
            "production traces do not cover every modeled transition family: "
            + ", ".join(missing)
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
