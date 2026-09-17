"""Qualify the real MCP realization crash traces against SessionRealizationMutex."""
import json
import os
from pathlib import Path
import tempfile

from formal_model_mutation_runner import ROOT, check_tlc_model_mutations


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def observation_field_paths(value, schema, path=()):
    """Walk every present wire field, including every array element/phase."""
    yield path, schema, value
    if isinstance(schema, tuple) and schema[0] is None:
        if value is None: return
        schema = schema[1]
    if isinstance(schema, dict):
        for key, child in schema.items(): yield from observation_field_paths(value[key], child, (*path, key))
    elif isinstance(schema, list):
        for index, child in enumerate(value): yield from observation_field_paths(child, schema[0], (*path, index))


def replace_observation_field(value, path, replacement=None, *, delete=False):
    from copy import deepcopy
    result = deepcopy(value)
    parent = result
    for key in path[:-1]: parent = parent[key]
    if delete: del parent[path[-1]]
    else: parent[path[-1]] = replacement
    return result


def check_realization_trace_mutants(tlc_jar: Path, trace_dir: Path, manifest: dict) -> None:
    from copy import deepcopy
    from render_runtime_refinement_traces import (render_observations, module_name, project_realization_trace,
        normalize_observation_times, observation_value, realization_event_schema, validate_observation_schema,
        GENERATION_SCHEMA, LEASE_SCHEMA, OBSERVATION_SET_FIELDS)
    name = "realization-http-crashes"
    original = json.loads((trace_dir / f"{name}.json").read_text())
    # Q8 cause/effect graph: phase -> required wire fields -> shape rejection;
    # complete reference + exact root/plan -> admitted call/report; any changed
    # immutable coordinate or widened expiry -> rejection. Crossing every rule,
    # event occurrence, array element and recovery side prevents one witness
    # (formerly published-2) from substituting for the entire decision table.
    # Q9 null is a fault only for NON-nullable fields. Root legacy optionals
    # retain their wire meaning. Unknown fields/types never become default facts.
    # Q10 deletion/duplication/swapping applies to each reachable phase boundary;
    # swapping two same-phase independent calls is a positive permutation case.
    # Q11 same-generation root renewal is positive: shorter existing evidence
    # remains admissible. Reject-all and equality-only "fixes" must fail here.
    schema_killed, semantic_cases, positives, report = 0, [], [], []
    bases = {}
    def projected(events):
        return normalize_observation_times([{"trace": project_realization_trace(events)}])[0]["trace"]
    def expression(step, events):
        before, after = bases[step["rule"]][1], projected(events)
        changes = []
        if len(before) == len(after):
            for i, (left, right) in enumerate(zip(before, after), 1):
                for key in right:
                    if left[key] != right[key]:
                        changes.append(f'![{i}].{key} = {observation_value(right[key], as_set=key in OBSERVATION_SET_FIELDS)}')
            value = bases[step["rule"]][0]
            if changes: value = "[" + value + " EXCEPT " + ", ".join(changes) + "]"
        else: value = observation_value(after)
        return f'RealizationTraceIsRefinement({value}, {observation_value(step["cut"])}, {step["count"]})'
    for index, step in enumerate(original["steps"]):
        events = step["trace"]
        bases[step["rule"]] = (f"Base{index}", projected(events))
        positives.append(expression(step, events))
        for event_index, event in enumerate(events):
            schema = realization_event_schema(event)
            for path, field_schema, value in observation_field_paths(event, schema):
                if not path: continue
                nullable = isinstance(field_schema, tuple) and field_schema[0] is None
                underlying = field_schema[1] if nullable else field_schema
                invalid = [] if isinstance(underlying, dict) else {} if isinstance(underlying, list) else True if underlying == "uint" else 0 if underlying == "bool" else False
                faults = [("type", invalid)] + ([] if nullable else [("null", None)])
                if underlying == "text": faults.append(("empty", ""))
                elif underlying == "uint": faults.extend([("negative", -1), ("overflow", 2**64), ("fraction", 1.5)])
                elif isinstance(underlying, tuple): faults.append(("unknown-enum", "unknown-observation"))
                elif isinstance(underlying, list) and value: faults.append(("duplicate-element", value + [value[0]]))
                if isinstance(path[-1], str): faults.append(("missing", None))
                for fault, replacement in faults:
                    mutant = replace_observation_field(event, path, replacement, delete=fault == "missing")
                    try: validate_observation_schema(mutant, realization_event_schema(mutant), "event")
                    except ValueError as error:
                        if "observation schema violation" not in str(error): raise
                        schema_killed += 1
                    else: raise SystemExit(f"schema mutant survived: {step['rule']}/{event_index}/{path}/{fault}")
                if isinstance(underlying, dict):
                    mutant = replace_observation_field(event, path, dict(value or {}, unexpected_observation=True))
                    try: validate_observation_schema(mutant, realization_event_schema(mutant), "event")
                    except ValueError as error:
                        if "observation schema violation" not in str(error): raise
                        schema_killed += 1
                    else: raise SystemExit(f"unknown-field mutant survived: {step['rule']}/{event_index}/{path}")
                if field_schema == [GENERATION_SCHEMA]:
                    # Q13 collection completeness is checked even when a crash
                    # prevents the omitted second effect from executing.
                    for item in range(len(value)):
                        mutated = deepcopy(events)
                        mutated[event_index] = replace_observation_field(event, (*path, item), delete=True)
                        semantic_cases.append((step, mutated, f"{step['rule']}/{event_index}/{path}/missing-element-{item}"))
                reference = field_schema == GENERATION_SCHEMA
                asserted_lease = field_schema == LEASE_SCHEMA and event["point"] in {"projection", "stage", "published", "complete"}
                if reference or asserted_lease:
                    # A reference's generation is itself a coordinate record
                    # (attachment, version, content digest): change each of its
                    # leaves, never the record as a whole, so every case stays
                    # well-formed and reaches TLC instead of the shape check.
                    leaves = [((field, sub), old) for field, record in value.items() if isinstance(record, dict)
                              for sub, old in record.items()]
                    leaves += [((field,), old) for field, old in value.items() if not isinstance(old, dict)]
                    for fields, old in leaves:
                        new = old + (999999999 if "expires" in fields[-1] else 999) if type(old) is int else "foreign-observation"
                        mutated = deepcopy(events)
                        mutated[event_index] = replace_observation_field(event, (*path, *fields), new)
                        semantic_cases.append((step, mutated, f"{step['rule']}/{event_index}/{path}/{'.'.join(fields)}"))
            for field in ("sequence", "session"):
                mutated = deepcopy(events)
                mutated[event_index][field] = event[field] + 999 if field == "sequence" else "foreign-session"
                semantic_cases.append((step, mutated, f"{step['rule']}/{event_index}/{field}"))
            for operation in ("delete", "duplicate", "swap"):
                if operation == "swap" and event_index + 1 == len(events): continue
                mutated = deepcopy(events)
                if operation == "delete": del mutated[event_index]
                elif operation == "duplicate": mutated.insert(event_index, deepcopy(event))
                else: mutated[event_index], mutated[event_index + 1] = mutated[event_index + 1], mutated[event_index]
                for i, item in enumerate(mutated): item["sequence"] = i
                label = f"{step['rule']}/{event_index}/{operation}"
                if operation == "swap" and event["point"] == events[event_index + 1]["point"]:
                    positives.append(expression(step, mutated))
                else: semantic_cases.append((step, mutated, label))
        renewed = deepcopy(events)
        for event in renewed:
            data = event["data"]
            if "attachments" in data:
                if data["lease"]: data["lease"]["expires_at_unix_ms"] += 1000
                for attachment in data["attachments"]:
                    if attachment["claim"]: attachment["claim"]["lease_expires_at_unix_ms"] += 1000
        positives.append(expression(step, renewed))

    artifact_root = os.environ.get("AWAKEN_FORMAL_MUTATION_EVIDENCE_DIR")
    with tempfile.TemporaryDirectory(prefix="awaken-observation-matrix-") as temp:
        raw, rendered = Path(temp) / "raw", Path(temp) / "rendered"
        raw.mkdir()
        write_json(raw / f"{name}.json", original)
        render_observations(raw, rendered, ROOT, manifest, False)
        module = module_name(name)
        config = (rendered / f"{module}.cfg").read_text()
        declarations = "\n".join(f"{symbol} == {observation_value(trace)}" for symbol, trace in bases.values())
        prelude = f"---- MODULE {module} ----\nEXTENDS SessionRealizationMutex\n" + declarations + "\nTraceInit == Init\nTraceNext == UNCHANGED vars\nTraceSpec == TraceInit /\\ [][TraceNext]_vars\n"
        def model(expressions): return prelude + "TraceRefines == /\\ " + "\n    /\\ ".join(expressions) + "\n" + "=" * 77 + "\n"
        check_tlc_model_mutations(tlc_jar, module, model(positives), config, [], dependencies=("SessionRealizationMutex",), artifact_directory=Path(artifact_root) / "positive-controls" if artifact_root else None)
        negatives = []
        # Q12 independently exercise the model's required-authority guard even
        # though v3 schema rejects missing raw leases before model projection.
        for step in original["steps"]:
            symbol, states = bases[step["rule"]]
            for index, state in enumerate(states, 1):
                if state["event"] not in {"begin-before", "crash", "publish-one", "drain-one"}:
                    negatives.append(f'~RealizationTraceIsRefinement([{symbol} EXCEPT ![{index}].hasauthority = FALSE], {observation_value(step["cut"])}, {step["count"]})')
                    report.append({"case": f"{step['rule']}/{index - 1}/required-authority", "oracle": "TLC-projection-refutation"})
        for step, events, label in semantic_cases:
            try: expr = expression(step, events)
            except ValueError as error:
                if "must begin with an observed root" not in str(error): raise
                report.append({"case": label, "oracle": "missing-initial-root"})
                continue
            if expr == expression(step, step["trace"]): raise SystemExit(f"erased semantic mutant: {label}")
            negatives.append("~(" + expr + ")")
            report.append({"case": label, "oracle": "TLC-refutation"})
        # Each conjunction asserts rejection of EVERY counterfactual, never
        # "at least one fails". The shared oracle requires clean TLC completion;
        # parser/tool/timeout errors and any surviving case fail the gate.
        if artifact_root:
            write_json(Path(artifact_root) / "cases.json", {"schema_rejections": schema_killed, "positive_controls": len(positives), "cases": report})
        for offset in range(0, len(negatives), 64):
            source = model(negatives[offset:offset + 64])
            check_tlc_model_mutations(tlc_jar, module, source, config, [], dependencies=("SessionRealizationMutex",),
                artifact_directory=Path(artifact_root) / f"batch-{offset // 64}" if artifact_root else None)
    print(f"Observation schema mutations: {schema_killed}/{schema_killed} rejected")
    print(f"Continuous observation matrix: {len(report)}/{len(report)} rejected; {len(positives)} positive controls passed; {len(bases)} rules")
