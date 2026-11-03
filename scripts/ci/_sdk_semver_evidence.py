"""Pure observations and downstream inputs for the SDK release boundary.

The existing SDK qualifier owns source acquisition and every process. This
module parses the pinned tool's complete observations and renders the already
reviewed compile-only downstream cases; it runs no command and keeps no state.
"""
from __future__ import annotations

import json
import hashlib
import re
from collections import Counter
from pathlib import Path

COHORT = {
    "awaken-ext-builtin-tools": ("1.0.0", "2.0.0"),
    "awaken-provider-genai": ("1.0.0", "2.0.0"),
    "awaken-runtime": ("1.0.0", "2.0.0"),
    "awaken-runtime-contract": ("1.0.0", "2.0.0"),
    "awaken-agent-contract": ("1.0.0", "2.0.0"),
    "awaken-ext-background-task": ("1.0.0", "2.0.0"),
    "awaken-ext-compact": ("1.0.0", "2.0.0"),
    "awaken-ext-goal": ("1.0.0", "2.0.0"),
    "awaken-ext-mcp": ("0.1.0", "0.2.0"),
    "awaken-mcp-wire": ("0.1.0", "0.2.0"),
    "awaken-ext-memory": ("1.0.0", "2.0.0"),
    "awaken-ext-permission": ("1.0.0", "2.0.0"),
    "awaken-ext-skills": ("0.1.0", "0.2.0"),
    "awaken-ext-state-machine": ("0.1.0", "0.2.0"),
    "awaken-observability": ("1.0.0", "2.0.0"),
    "awaken-store-inmem": ("1.0.0", "2.0.0"),
    "awaken-tool-contract": ("1.0.0", "2.0.0"),
}
# Reviewed cargo-semver-checks0.47 default/all forced-patch observations:
# a2cb0cc9b92f03062058ea88c55c97cc0ad65ced versus50b3f3e22dbfe1d91dd6ee6fef8ee8dfc8e15940.
# Four Web reports reflect definition relocation: the original Builtin root
# paths, variants, fields and serde remain, with both old/current Web controls0.
# selected_hand_recovery_modes is a real removed SDK API, acknowledged by2.0.
# Provider's empty findings are a tool blindspot, not compatibility: its original
# enum/Client E0004/E0308 and successful migration controls remain mandatory.
# Fresh H395 (395303e5) default/all pinned0.47 raw reports versus the same
# 50b3 baseline additionally record the v5 Credential/Goal/Memory/Tool/Contract
# breaks below. Repeated entries are actual public-export occurrences, not
# normalized away; audit acceptance requires their exact reviewed multiplicity.
# McpWire's declared 0.2 ToolFault enum break is independently observed with
# 222 nonempty forced-patch queries in each profile (ADR-0103), so its normal
# major/zero-query observation cannot escape the same cohort audit boundary.
# These data never replace nonempty queries, exact source/profile receipts, or
# raw statuses; an unestablished/unknown set still refuses in the same parser.
# Runtime/Contract findings were independently observed in the pinned0.47
# default/all reports for 7df94e4654 versus 50b3, with 222 nonempty checks each.
# The SDK normal/build source closure is unchanged through f5316e0dbc. Record
# their exact already-declared 2.0 removals, never an empty or predicted set.
# Actual consumer controls and fresh source/profile receipts remain required.
# The complete f5316e0dbc default/all22 forced reports additionally observed
# Agent's Disposition:StopRouting. Real baseline/current/migrated compiler
# inputs showed the E0004 enum break and E0277 public Contract identity break.
# Public Agent/Contract carriers therefore join the explicit breaking release;
# their empty pinned-tool findings are a blindspot, not a compatibility claim.
# All declarations still need fresh source/profile-bound nonempty reports and
# the original compiler/archive/consumer evidence; unknown/extra items refuse.
APPROVED_FINDINGS = {
    "awaken-ext-builtin-tools": {
        ("enum_missing", "enum", "awaken_ext_builtin_tools::WebDomainFilter"),
        ("function_missing", "function", "awaken_ext_builtin_tools::selected_hand_recovery_modes"),
        ("struct_missing", "struct", "awaken_ext_builtin_tools::WebFetchExecutionConfiguration"),
        ("struct_missing", "struct", "awaken_ext_builtin_tools::WebSearchUserLocation"),
        ("struct_missing", "struct", "awaken_ext_builtin_tools::WebSearchExecutionConfiguration"),
    },
    "awaken-provider-genai": set(),
    "awaken-runtime": {
        ("inherent_method_missing", "method", "Runtime::session"),
        ("inherent_method_missing", "method", "Runtime::with_max_consecutive_inference_failures"),
        ("struct_missing", "struct", "awaken_runtime::EmbeddedSession"),
    },
    "awaken-runtime-contract": (
        ("constructible_struct_adds_field", "field", "RuntimeRunContext.cancellation_cause"),
        ("constructible_struct_adds_field", "field", "RuntimeRunContext.cancellation_cause"),
        ("enum_variant_added", "variant", "EditError:Unavailable"),
        ("enum_variant_added", "variant", "Error:Admission"),
        ("enum_variant_added", "variant", "Offer:Full"),
        ("enum_variant_added", "variant", "WaitOutcome:Unavailable"),
        ("module_missing", "mod", "awaken_runtime_contract::credential"),
        ("pub_module_level_const_missing", "const", "THREAD_USAGE_STATE_KEY"),
        ("struct_missing", "struct", "awaken_runtime_contract::llm::ThreadUsage"),
        ("struct_missing", "struct", "awaken_runtime_contract::llm::ThreadUsageKey"),
        ("trait_method_missing", "method", "next_client_execution of trait OwnedToolContinuation"),
    ),
    "awaken-agent-contract": {
        ("constructible_struct_adds_field", "field", "CredentialExecutionPolicy.handling_mode"),
        ("enum_variant_added", "variant", "Error:CapacityExhausted"),
        ("enum_variant_added", "variant", "Disposition:StopRouting"),
    },
    "awaken-ext-background-task": {
        ("enum_missing", "enum", "awaken_ext_background_task::BackgroundTaskEnd"),
        ("enum_missing", "enum", "awaken_ext_background_task::BackgroundTaskError"),
        ("enum_missing", "enum", "awaken_ext_background_task::BackgroundWait"),
        ("pub_module_level_const_missing", "const", "BACKGROUND_TASK_STATE_PREFIX"),
        ("struct_missing", "struct", "awaken_ext_background_task::BackgroundTaskId"),
        ("struct_missing", "struct", "awaken_ext_background_task::BackgroundTaskScope"),
        ("struct_missing", "struct", "awaken_ext_background_task::BackgroundTaskCompletion"),
        ("struct_missing", "struct", "awaken_ext_background_task::BackgroundTaskWaitCandidate"),
    },
    "awaken-ext-compact": set(),
    "awaken-ext-goal": {
        ("auto_trait_impl_removed", "type", "Error is no longer UnwindSafe"),
        ("auto_trait_impl_removed", "type", "Error is no longer RefUnwindSafe"),
        ("derive_trait_impl_removed", "type", "Error no longer derives PartialEq"),
        ("derive_trait_impl_removed", "type", "Error no longer derives Eq"),
        ("enum_variant_added", "variant", "Error:Admission"),
    },
    "awaken-mcp-wire": {
        ("enum_variant_added", "variant", "McpTransportError:ToolFault"),
    },
    "awaken-ext-mcp": set(),
    "awaken-ext-memory": (
        ("constructible_struct_adds_field", "field", "MemoryTerminalExtractionRequest.execution"),
        ("constructible_struct_adds_field", "field", "MemoryTerminalExtractionRequest.execution"),
        ("constructible_struct_adds_private_field", "field", "MemoryExtractionIntent.execution"),
        ("constructible_struct_adds_private_field", "field", "MemoryExtractionIntent.execution"),
        ("enum_variant_added", "variant", "MemoryExtractionError:Unavailable"),
        ("enum_variant_added", "variant", "MemoryExtractionError:Unavailable"),
    ),
    "awaken-ext-permission": set(),
    "awaken-ext-skills": set(),
    "awaken-ext-state-machine": set(),
    "awaken-observability": set(),
    "awaken-store-inmem": set(),
    "awaken-tool-contract": (
        ("enum_variant_added", "variant", "ToolCallPhase:Returned"),
        ("enum_variant_added", "variant", "ToolCallPhase:KnownAborted"),
        ("enum_variant_added", "variant", "ToolCallPhase:Returned"),
        ("enum_variant_added", "variant", "ToolCallPhase:KnownAborted"),
    ),
}
GENAI_GIT = "https://github.com/awakenworks/rust-genai.git"
GENAI_REV = "da7b568fb413b3197acc13c93f334884f97c81ad"
GENAI_SOURCES = {
    False: {"source": f"git+{GENAI_GIT}?rev={GENAI_REV}#{GENAI_REV}", "version": "0.7.0-beta.21"},
    True: {"source": "registry+https://github.com/rust-lang/crates.io-index", "version": "0.7.0-rc.8"},
}


def observations(output: str, expected: list[str], *, strict=False) -> dict[str, dict]:
    output = re.sub(r"\x1b\[[0-9;]*m", "", output)
    packages, current, lint, failed_in = {}, None, None, False
    for line in output.splitlines():
        if strict and re.match(r"\s*(?:error(?:\[[A-Za-z0-9]+\])?:|Error:|fatal:|"
                               r"thread ['\"].+['\"] panicked|panicked at|Caused by:|"
                               r"stack backtrace:|Traceback \(|failed to |"
                               r"process .+didn't exit successfully)", line):
            raise ValueError("SDK major report contains a tool/compiler failure")
        start = re.fullmatch(r"\s*Checking ([A-Za-z0-9_-]+) v([^ ]+) -> v([^ ]+) \(([^)]+)\)", line)
        checked = re.fullmatch(r"\s*Checked \[[^\]]+\] (\d+) checks: (\d+) pass(?:, (\d+) fail)?(?:, (\d+) warn)?(?:, (\d+) skip)?", line)
        failure = re.fullmatch(r"--- failure ([A-Za-z0-9_]+): .* ---", line)
        tagged = re.fullmatch(r"  ([A-Za-z_-]+) (.+), previously in file .+", line)
        method = re.fullmatch(r"  (\S+::\S+), previously in file .+", line)
        variant = re.fullmatch(r"  variant (\S+) in .+:\d+", line)
        if lint == "inherent_method_missing":
            finding = ("method", method[1]) if method else None
        elif lint == "enum_variant_added":
            finding = ("variant", variant[1]) if variant else None
        elif lint in ("constructible_struct_adds_field", "constructible_struct_adds_private_field"):
            field = re.fullmatch(r"  field (\S+) in .+:\d+", line)
            finding = ("field", field[1]) if field else None
        elif lint in ("auto_trait_impl_removed", "derive_trait_impl_removed"):
            grammar = (r"  type (.+ is no longer \S+), in .+:\d+"
                       if lint == "auto_trait_impl_removed" else
                       r"  type (.+ no longer derives \S+), in .+:\d+")
            removed = re.fullmatch(grammar, line)
            finding = ("type", removed[1]) if removed else None
        elif lint == "pub_module_level_const_missing":
            constant = re.fullmatch(r"  (\S+) in file .+:\d+", line)
            finding = ("const", constant[1]) if constant else None
        else:
            finding = tagged.groups() if tagged else None
        summary = re.fullmatch(r"\s*Summary (.+)", line)
        finished = re.fullmatch(r"\s*Finished \[[^\]]+\] ([A-Za-z0-9_-]+)", line)
        if start:
            name, old, new, change = start.groups()
            if current is not None or name in packages:
                raise ValueError("SDK SemVer package is unfinished or duplicated")
            current = dict(package=name, old_version=old, new_version=new,
                           version_change=change, findings=[], failed_lints=[])
            lint, failed_in = None, False
        elif checked:
            if current is None or "checks" in current:
                raise ValueError("SDK SemVer checked observation is missing or duplicated")
            current.update(zip(("checks", "passed", "failed", "warned", "skipped"),
                               (int(value or 0) for value in checked.groups())))
            if current["checks"] != sum(current[key] for key in ("passed", "failed", "warned")):
                raise ValueError("SDK SemVer check totals disagree")
        elif failure:
            lint = failure[1]
            failed_in = False
            if current is None or lint in current["failed_lints"]:
                raise ValueError("SDK SemVer lint is unowned or duplicated")
            current["failed_lints"].append(lint)
        elif line == "Failed in:":
            if current is None or lint is None or failed_in:
                raise ValueError("SDK SemVer findings block is unowned or duplicated")
            failed_in = True
        elif finding:
            if current is None or lint is None or "summary" in current:
                raise ValueError("SDK SemVer finding has no package or lint")
            item = (lint, *finding)
            # Pinned 0.47 can report one source item through multiple public
            # exports. Preserve every physical occurrence; only the exact
            # reviewed multiset in validate_audit can accept that multiplicity.
            current["findings"].append(item)
        elif summary:
            failed_in = False
            if strict and (current is None or "summary" in current):
                raise ValueError("SDK major summary has no package or is duplicated")
            if current is not None:
                current["summary"] = summary[1]
        elif finished:
            if current is None or current["package"] != finished[1] or "checks" not in current:
                raise ValueError("SDK SemVer finished observation lacks its checked package")
            if (current["failed"] != len(current["failed_lints"])
                    or set(current["failed_lints"]) != {item[0] for item in current["findings"]}):
                raise ValueError("SDK SemVer failed checks lack complete findings")
            if strict:
                actual_summary = current.get("summary")
                if current["failed"]:
                    counts = re.fullmatch(r"semver requires new (major|minor) version: "
                                          r"(\d+) major and (\d+) minor checks failed", actual_summary or "")
                    if (counts is None or int(counts[2]) + int(counts[3]) != current["failed"]
                            or counts[1] != ("major" if int(counts[2]) else "minor")):
                        raise ValueError("SDK major summary disagrees with failed lint counts")
                elif actual_summary != "no semver update required":
                    raise ValueError("SDK major report has no successful pinned summary")
            packages[current["package"]] = current
            current, lint, failed_in = None, None, False
        elif strict and failed_in and line.strip():
            raise ValueError("SDK SemVer findings block contains an unrecognized row")
        elif strict and re.match(r"\s*(?:Checking |Checked |Summary |Finished |--- failure )", line):
            raise ValueError("SDK major report has a malformed pinned observation")
    if current is not None or len(packages) != len(expected) or set(packages) != set(expected):
        raise ValueError("SDK SemVer package execution is incomplete")
    return packages


def validate_standard(output: str, expected: list[str], major_audit=None) -> dict[str, dict]:
    rows = observations(output, expected)
    for name, row in rows.items():
        if row["failed"] or row["warned"]:
            raise ValueError("SDK SemVer has unsuccessful checks")
        if row["checks"]:
            continue
        audited = (major_audit or {}).get(name)
        if (COHORT.get(name) != (row["old_version"], row["new_version"])
                or row["version_change"] != "major change" or not audited
                or audited.get("checks", 0) <= 0
                or (audited.get("old_version"), audited.get("new_version")) != COHORT[name]):
            raise ValueError("SDK SemVer empty checks have no complete nonempty major audit")
    return rows


def validate_audit(output: str, original_status: int, *, approved=APPROVED_FINDINGS) -> dict[str, dict]:
    rows = observations(output, sorted(COHORT), strict=True)
    if approved is None or set(approved) != set(COHORT):
        raise ValueError("SDK major findings have not been established from the actual report")
    for name, row in rows.items():
        if ((row["old_version"], row["new_version"]) != COHORT[name]
                or row["version_change"] != "assume patch change"
                or row["checks"] <= 0 or row["warned"]
                or Counter(row["findings"]) != Counter(approved[name])):
            raise ValueError(f"SDK major audit is incomplete or has unreviewed changes: {name}")
    required_status = int(any(row["failed"] for row in rows.values()))
    if original_status != required_status:
        raise ValueError("SDK major report disagrees with the original tool status")
    return rows


def validate_compiler_case(output: str, status: int, source: str, file_name: str,
                           expected_code: str | None, marker: str) -> list[dict]:
    messages, finished = [], []
    for line in output.splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(value, dict):
            continue
        if value.get("reason") == "build-finished":
            finished.append(value.get("success"))
        if value.get("reason") == "compiler-message":
            if not isinstance(value.get("message"), dict):
                raise ValueError("SDK major compiler diagnostic is malformed")
            if value["message"].get("level") == "error":
                messages.append(value["message"])
    if expected_code is None:
        if status != 0 or messages or finished != [True]:
            raise ValueError("SDK major migration or baseline positive control did not compile")
        return []
    matching_lines = [number for number, line in enumerate(source.splitlines(), 1) if marker in line]
    if status != 101 or len(messages) != 1 or len(matching_lines) != 1 or finished != [False]:
        raise ValueError("SDK major negative control has no exact compiler counterexample")
    message = messages[0]
    spans = [span for span in message.get("spans", []) if span.get("is_primary")]
    if ((message.get("code") or {}).get("code") != expected_code or len(spans) != 1
            or any(span.get("file_name", "").replace("\\", "/") != file_name
                   or span.get("line_start") != matching_lines[0] for span in spans)):
        raise ValueError("SDK major compiler code or original downstream span differs")
    return messages


# Defining-owner roles require a direct normal compiler dependency even when
# a package also belongs to the separately declared release cohort.
# Versions and enabled features come from each side's original SDK metadata.
WITNESS_OWNERS = ("awaken-agent-contract", "awaken-tool-contract")


def consumer_package_names() -> tuple[str, ...]:
    """Render each package once while preserving its independent policy roles."""
    return tuple(dict.fromkeys((*COHORT, *WITNESS_OWNERS)))


def consumer_manifest(sdk_packages: dict, *, current: bool, feature: str) -> str:
    """Render only compile-only inputs, never a production compatibility path."""
    text = ('[package]\nname="sdk-major-observation"\nversion="0.0.0"\nedition="2024"\n'
            '[workspace]\n[dependencies]\n')
    for name in consumer_package_names():
        package = sdk_packages[name]
        directory = Path(package['manifest_path']).parent
        version = COHORT[name][int(current)] if name in COHORT else package["version"]
        features = sorted(package['features']) if feature == 'all' else []
        text += (f'{name} = {{ path = {json.dumps(str(directory))}, version = "={version}", '
                 f'features = {json.dumps(features)} }}\n')
    text += ('genai_old = { package="genai", git=' + json.dumps(GENAI_GIT)
             + ', rev=' + json.dumps(GENAI_REV) + ' }\n')
    if current:
        text += 'genai = "=' + GENAI_SOURCES[True]['version'] + '"\n'
    for name in witness_cases(current):
        text += f'\n[[bin]]\nname="{name}"\npath="cases/{name}.rs"\n'
    return text


def witness_cases(current: bool) -> dict[str, tuple[str | None, str]]:
    if not current:
        return {name: (None, "") for name in (
            "old_enum", "old_client", "old_builtin", "web_paths",
            "old_runtime_type", "old_runtime_method", "old_retry_policy_decide",
            "owner_legacy_inventory", "owner_old_message_id", "owner_old_credential_module",
            "owner_old_credential_root", "owner_old_tool_module", "owner_old_tool_root",
            "owner_canonical",
            "inference_old_disposition", "inference_old_error", "inference_old_gate",
            "inference_old_runtime_ceiling", "mcp_old_transport_error",
        )}
    return {
        "old_enum": ("E0004", "match adapter {"),
        "old_client": ("E0308", "GenaiExecutor::with_client(client)"),
        "old_builtin": ("E0425", "awaken_ext_builtin_tools::selected_hand_recovery_modes"),
        "migrated_provider": (None, ""),
        "web_paths": (None, ""),
        "old_runtime_type": ("E0425", "Option<awaken_runtime::EmbeddedSession"),
        "old_runtime_method": ("E0599", "runtime.session("),
        "migrated_runtime": (None, ""),
        "old_retry_policy_decide": ("E0308", "ModelExecutionEvidence::Unknown"),
        "migrated_retry_policy_decide": (None, ""),
        # Designed compiler controls only; original capture records actual codes/statuses.
        "owner_old_message_id": ("E0425", "awaken_runtime_contract::MessageId"),
        "owner_old_credential_module": ("E0433", "awaken_runtime_contract::credential::CredentialRef"),
        "owner_old_credential_root": ("E0425", "awaken_runtime_contract::CredentialRef"),
        "owner_old_tool_module": ("E0433", "awaken_runtime_contract::tool::ToolOutput"),
        "owner_old_tool_root": ("E0425", "awaken_runtime_contract::ToolOutput"),
        "owner_canonical": (None, ""),
        # New-main source controls: expected codes only, never observed findings.
        "inference_old_disposition": ("E0004", "match disposition {"),
        "inference_migrated_disposition": (None, ""),
        "inference_old_error": ("E0004", "match error {"),
        "inference_migrated_error": (None, ""),
        "inference_old_gate": ("E0308", "gate.admit_model_request(request).await"),
        "inference_migrated_gate": (None, ""),
        "inference_old_runtime_ceiling": ("E0599", "runtime.with_max_consecutive_inference_failures(3)"),
        "inference_migrated_runtime_policy": (None, ""),
        # ADR-0103: the original exhaustive wire error match compiles at the
        # baseline, refuses ToolFault with E0004 now, and migrates explicitly.
        "mcp_old_transport_error": ("E0004", "match error {"),
        "mcp_migrated_transport_error": (None, ""),
    }


def validate_consumer_metadata(metadata: dict, consumer: Path, sdk_packages: dict, *, current: bool, feature: str) -> dict:
    if (Path(metadata["workspace_root"]).resolve() != consumer.resolve()
            or len(metadata["workspace_members"]) != 1):
        raise ValueError("major consumer is not its independent one-member workspace")
    packages = metadata["packages"]
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    by_id = {package["id"]: package for package in packages}
    roots = metadata["workspace_members"]
    if (len(nodes) != len(metadata["resolve"]["nodes"]) or len(by_id) != len(packages)
            or set(nodes) != set(by_id) or roots[0] not in by_id
            or Path(by_id[roots[0]]["manifest_path"]).resolve() != (consumer / "Cargo.toml").resolve()):
        raise ValueError("major consumer graph has duplicate, incomplete or foreign identities")
    if any(not isinstance(node.get("features"), list)
           or any(not isinstance(value, str) for value in node["features"])
           or len(set(node["features"])) != len(node["features"]) for node in nodes.values()):
        raise ValueError("major consumer graph has malformed feature observations")
    reached, pending = set(), list(roots)
    while pending:
        identity = pending.pop()
        if identity in reached:
            continue
        reached.add(identity)
        for dependency in nodes[identity]["deps"]:
            if dependency["pkg"] not in nodes:
                raise ValueError("major consumer graph has an unresolved dependency")
            pending.append(dependency["pkg"])

    def genai_link(identity, alias, wanted_current):
        links = [dep["pkg"] for dep in nodes[identity]["deps"] if dep["name"] == alias]
        if len(links) != 1 or links[0] not in reached:
            raise ValueError("major consumer Client dependency alias is missing or ambiguous")
        package = by_id[links[0]]
        if package["name"] != "genai" or any(package.get(key) != value for key, value in GENAI_SOURCES[wanted_current].items()):
            raise ValueError("major consumer Client source is outside its declared migration")
        if "bedrock-sigv4" in nodes[package["id"]]["features"]:
            raise ValueError("major consumer graph does not satisfy the original enum input's feature condition")
        return package
    selected = {}
    for name in consumer_package_names():
        found = [package for package in packages if package["name"] == name]
        package = sdk_packages[name]
        version = COHORT[name][int(current)] if name in COHORT else package["version"]
        path = Path(package['manifest_path'])
        if (len(found) != 1 or found[0].get("source") is not None
                or Path(found[0]["manifest_path"]).resolve() != path.resolve()
                or found[0]["version"] != version
                or found[0].get("features") != package['features'] or found[0]['id'] not in reached):
            raise ValueError("major consumer SDK source/version/features differ from the declared cohort")
        if name in WITNESS_OWNERS:
            links = [edge for edge in nodes[roots[0]]["deps"]
                     if edge["name"] == name.replace("-", "_")]
            if (len(links) != 1 or links[0]["pkg"] != found[0]["id"]
                    or links[0].get("dep_kinds") != [{"kind": None, "target": None}]):
                raise ValueError("major consumer defining owner lacks its direct normal dependency")
        required = set(package['features']) if feature == 'all' else set()
        if "default" in package['features']:
            required.add("default")
        if not required.issubset(nodes[found[0]['id']]['features']):
            raise ValueError("major consumer did not enable the selected SDK dependency features")
        selected[name] = found[0]
    provider = selected["awaken-provider-genai"]
    linked = genai_link(provider["id"], "genai", current)
    original = genai_link(roots[0], "genai_old", False)
    if current and genai_link(roots[0], "genai", True)["id"] != linked["id"]:
        raise ValueError("major consumer migrated Client is not the Provider's registry type")
    if not current and original["id"] != linked["id"]:
        raise ValueError("major consumer baseline Client is not the Provider's Git type")
    return {"sdk_packages": {name: package["id"] for name, package in selected.items()},
            "requested_sdk_features": {name: sorted(package['features']) if feature == 'all' else []
                                       for name, package in sdk_packages.items() if name in consumer_package_names()},
            "resolved_sdk_features": {name: nodes[package['id']]['features'] for name, package in selected.items()},
            "genai": {key: linked[key] for key in ("id", "source", "version")},
            "genai_features": nodes[linked["id"]]["features"]}


def witness_steps(root: Path, work: Path, baseline: str, feature: str, target: str) -> list[dict]:
    """The existing action's argv, shared by execution and receipt validation."""
    steps = [{"name": "baseline-archive", "command": ["git", "archive", "--format=tar", "--output", str(work / "baseline.tar"), baseline], "exit_code": 0}]
    extra = ["--all-features"] if feature == "all" else []
    for current, sdk_root in ((False, work / "baseline-source"), (True, root)):
        side = "current" if current else "baseline"
        consumer = work / (side + "-consumer")
        steps.append({"name": side + "-sdk-metadata", "command": ["cargo", "metadata", "--manifest-path", str(sdk_root / "Cargo.toml"), "--locked", "--format-version", "1", *extra], "exit_code": 0})
        steps.append({"name": side + "-consumer-metadata", "command": ["cargo", "metadata", "--manifest-path", str(consumer / "Cargo.toml"), "--format-version", "1", *extra], "exit_code": 0})
        for name, (code, _) in witness_cases(current).items():
            steps.append({"name": side + "-" + name, "command": ["cargo", "check", "--locked", "--manifest-path", str(consumer / "Cargo.toml"), "--target", target, "--bin", name, "--message-format=json", *extra], "exit_code": 101 if code else 0})
    return steps


def validate_witness_receipt(receipt: dict, source: dict, baseline: str, feature: str, *,
                             target=None, root=None, work=None, side_facts=None) -> None:
    if (receipt.get('source') != source or receipt.get('baseline_rev') != baseline
            or receipt.get('feature') != feature or receipt.get('exit_code') != 0
            or target is None or root is None or work is None or receipt.get('target') != target
            or set(receipt.get('sides', {})) != {'baseline', 'current'}
            or not re.fullmatch(r'[0-9a-f]{64}', receipt.get('baseline_archive_sha256', ''))):
        raise ValueError('SDK major consumer receipt has a foreign or incomplete source')
    if side_facts is None or receipt['sides'] != side_facts:
        raise ValueError('SDK major consumer receipt differs from retained dependency facts')
    for current, side in ((False, 'baseline'), (True, 'current')):
        facts = side_facts[side]
        if (set(facts['sdk_packages']) != set(consumer_package_names())
                or set(facts['requested_sdk_features']) != set(consumer_package_names())
                or set(facts['resolved_sdk_features']) != set(consumer_package_names())
                or any(facts['genai'].get(key) != value for key, value in GENAI_SOURCES[current].items())
                or not isinstance(facts['genai'].get('id'), str) or not facts['genai']['id']
                or 'bedrock-sigv4' in facts['genai_features']
                or any(not re.fullmatch(r'[0-9a-f]{64}', facts.get(key, '')) for key in ('manifest_sha256', 'lock_sha256'))):
            raise ValueError('SDK major consumer side facts are incomplete')
    planned = witness_steps(root, work, baseline, feature, target)
    if len(receipt.get('steps', [])) != len(planned):
        raise ValueError('SDK major consumer step evidence is incomplete')
    for row, step in zip(receipt['steps'], planned):
        if (row.get('command') != step['command'] or row.get('exit_code') != step['exit_code']
                or row.get('log') != str(work / (step['name'] + '.log'))
                or not re.fullmatch(r'[0-9a-f]{64}', row.get('log_sha256', ''))
                or any(key in row for key in ('error', 'subject_error', 'record_error', 'assessment_error', 'semantic_result'))
                or row.get('result_exit_code', row['exit_code']) != row['exit_code']):
            raise ValueError('SDK major consumer step differs from its original command/status')
    expected = {(side, name): (code, marker)
                for current, side in ((False, 'baseline'), (True, 'current'))
                for name, (code, marker) in witness_cases(current).items()}
    seen = set()
    for case in receipt.get('cases', []):
        key = (case['side'], case['case'])
        if key not in expected or key in seen:
            raise ValueError('SDK major consumer cases are missing, extra or duplicated')
        seen.add(key)
        code, marker = expected[key]
        if (case['original_exit_code'] != (101 if code else 0) or case['expected_code'] != code
                or case['source_sha256'] != hashlib.sha256(WITNESS_SOURCES[key[1]].encode()).hexdigest()
                or len(case['compiler_errors']) != (1 if code else 0)):
            raise ValueError('SDK major consumer result does not bind its exact original input')
        diagnostics = [{'reason': 'compiler-message', 'message': message}
                       for message in case['compiler_errors']]
        diagnostics.append({'reason': 'build-finished', 'success': code is None})
        validate_compiler_case('\n'.join(json.dumps(value) for value in diagnostics),
            case['original_exit_code'], WITNESS_SOURCES[key[1]],
            'cases/' + key[1] + '.rs', code, marker)
    if seen != set(expected):
        raise ValueError('SDK major consumer did not execute every baseline and migration control')


WITNESS_SOURCES = {
    'old_enum': r'''// Public-dependency compatibility cause/effect design (compile-only downstream).
// Causes: P selects old Git SDK or frozen registry SDK; D is genai default features
// (bedrock-sigv4 disabled in both independent graphs); M is the complete old public
// AdapterKind match without a wildcard. Effect: old supported source must compile.
// Rules E1 P=old,D,M -> compile; E2 P=candidate,D,M -> compile if backwards
// compatible. Added exhaustive enum variants must surface as original E0004,
// not be hidden behind a wildcard, a replacement enum, or a snapshot bless.
// Every old default variant is listed from da7b568/adapter_kind.rs. Custom(u8)
// binds its payload; no wildcard arm exists. Actual compile status is recorded
// separately from this fixture and is not manufactured as an expected pass.
use awaken_provider_genai::AdapterKind;

pub fn old_adapter_code(adapter: AdapterKind) -> u8 {
    match adapter {
        AdapterKind::OpenAI => 0,
        AdapterKind::OpenAIResp => 1,
        AdapterKind::Gemini => 2,
        AdapterKind::GeminiIx => 3,
        AdapterKind::Anthropic => 4,
        AdapterKind::Fireworks => 5,
        AdapterKind::Together => 6,
        AdapterKind::Groq => 7,
        AdapterKind::Aihubmix => 8,
        AdapterKind::Kimi => 9,
        AdapterKind::Mimo => 10,
        AdapterKind::Moonshot => 11,
        AdapterKind::Nebius => 12,
        AdapterKind::Xai => 13,
        AdapterKind::DeepSeek => 14,
        AdapterKind::Zai => 15,
        AdapterKind::BigModel => 16,
        AdapterKind::Aliyun => 17,
        AdapterKind::QwenCloud => 18,
        AdapterKind::Baidu => 19,
        AdapterKind::Cohere => 20,
        AdapterKind::Ollama => 21,
        AdapterKind::OllamaCloud => 22,
        AdapterKind::Omlx => 23,
        AdapterKind::Vertex => 24,
        AdapterKind::GithubCopilot => 25,
        AdapterKind::OpenCodeGo => 26,
        AdapterKind::BedrockApi => 27,
        AdapterKind::OpenRouter => 28,
        AdapterKind::AtlasCloud => 29,
        AdapterKind::MiniMax => 30,
        AdapterKind::Custom(value) => value,
    }
}

fn main() {
    assert_eq!(old_adapter_code(AdapterKind::OpenAI), 0);
}
''',
    'old_client': r'''// Public-dependency compatibility cause/effect design (compile-only downstream).
// Causes: P selects old Git SDK or frozen registry SDK; C is a Client supplied
// by the original direct Git dependency pinned to da7b568. Effect: the public
// with_client constructor must accept the same previously valid client type.
// Rules C1 P=old,C -> compile (positive control); C2 P=candidate,C -> compile
// if backwards compatible. Nominal source identity changes must remain visible
// as the original E0308. This fixture does not execute a model or read credentials.
// The direct old dependency is a downstream observation, not a production dual
// genai compatibility path, registry override, wrapper type, or second adapter.
use awaken_provider_genai::GenaiExecutor;

pub fn accept_original_git_client(client: genai_old::Client) -> GenaiExecutor {
    GenaiExecutor::with_client(client)
}

fn main() {
    let _constructor: fn(genai_old::Client) -> GenaiExecutor = accept_original_git_client;
}
''',
    'migrated_provider': r'''// Formal migration observation source, prepared only; not yet executed.
// M1: consumer explicitly changes its direct original Git genai dependency to
// the same registry genai =0.7.0-rc.8 as Provider2.0. M2: it intentionally handles
// the four newly exposed exhaustive variants. Effects: this documented migrated
// source compiles while original1.0 A/B failures remain unchanged receipts.
// No wildcard arm, bridge Client, second enum, old Git genai source or model IO.
use awaken_provider_genai::{AdapterKind, GenaiExecutor};

pub fn migrated_registry_client(client: genai::Client) -> GenaiExecutor {
    GenaiExecutor::with_client(client)
}

pub fn adapter_code(adapter: AdapterKind) -> u8 {
    match adapter {
        AdapterKind::OpenAI => 0,
        AdapterKind::OpenAIResp => 1,
        AdapterKind::Gemini => 2,
        AdapterKind::GeminiIx => 3,
        AdapterKind::Anthropic => 4,
        AdapterKind::Fireworks => 5,
        AdapterKind::Together => 6,
        AdapterKind::Groq => 7,
        AdapterKind::Aihubmix => 8,
        AdapterKind::Kimi => 9,
        AdapterKind::Mimo => 10,
        AdapterKind::Moonshot => 11,
        AdapterKind::Nebius => 12,
        AdapterKind::Xai => 13,
        AdapterKind::DeepSeek => 14,
        AdapterKind::Mistral => 15,
        AdapterKind::Zai => 16,
        AdapterKind::BigModel => 17,
        AdapterKind::Aliyun => 18,
        AdapterKind::QwenCloud => 19,
        AdapterKind::Baidu => 20,
        AdapterKind::Cohere => 21,
        AdapterKind::Ollama => 22,
        AdapterKind::OllamaCloud => 23,
        AdapterKind::Omlx => 24,
        AdapterKind::Vertex => 25,
        AdapterKind::GithubCopilot => 26,
        AdapterKind::OpenCodeGo => 27,
        AdapterKind::BedrockApi => 28,
        AdapterKind::OpenRouter => 29,
        AdapterKind::AtlasCloud => 30,
        AdapterKind::Requesty => 31,
        AdapterKind::ApiRoute => 32,
        AdapterKind::Heabsy => 33,
        AdapterKind::MiniMax => 34,
        AdapterKind::Custom(value) => value,
    }
}
fn main() {
    let _constructor: fn(genai::Client) -> GenaiExecutor = migrated_registry_client;
    assert_eq!(adapter_code(AdapterKind::OpenAI), 0);
}
''',
    'old_builtin': r'''// Read-only downstream source design, not an executed qualification.
// C1 selects immutable 50b3 or current SDK; C2 calls the original SDK public
// function without changing its name. Effect: old source compiles. Old compile
// is the positive control; a current unresolved public item is a real source
// break. This case adds no duplicate catalog algorithm, Platform dependency,
// replacement public enum, migration shim, or SemVer exemption.
fn main() {
    let modes = awaken_ext_builtin_tools::selected_hand_recovery_modes(&[]);
    assert!(modes.is_empty());
}
''',
    'web_paths': r'''// Read-only downstream source design, not an executed qualification.
// C1 selects immutable 50b3 or current SDK; C2 names every original public Web
// root; C3 constructs all original fields/variants. Effect: identical source
// must compile. Positive controls must precede any candidate conclusion. No
// canonical-path rewrite, wildcard enum arm or duplicate type is permitted.
use awaken_ext_builtin_tools::{
    WebDomainFilter, WebFetchExecutionConfiguration, WebSearchExecutionConfiguration,
    WebSearchUserLocation,
};

pub fn old_domain_variants(value: WebDomainFilter) -> Vec<String> {
    match value {
        WebDomainFilter::Allow(domains) => domains,
        WebDomainFilter::Block(domains) => domains,
    }
}

pub fn original_fields() -> (WebFetchExecutionConfiguration, WebSearchExecutionConfiguration) {
    let location = WebSearchUserLocation {
        city: Some("Singapore".into()), country: Some("SG".into()),
        region: None, timezone: Some("Asia/Singapore".into()),
    };
    let fetch = WebFetchExecutionConfiguration {
        url_sources: None, domains: Some(WebDomainFilter::Allow(vec!["example.com".into()])),
        max_content_tokens: Some(0),
    };
    let search = WebSearchExecutionConfiguration {
        domains: Some(WebDomainFilter::Block(vec!["private.example.com".into()])),
        user_location: Some(location),
    };
    (fetch, search)
}
fn main() { let _ = original_fields(); }
''',
    'old_runtime_type': r'''// Runtime release cause/effect R1: the first pre-release source exposes the
// former opaque binding type -> the identical downstream compiles there.
// R2: canonical candidate deletes that name -> observed rustc E0425 at the
// single type span; retain raw101. This is a compile-only negative input,
// never a production alias or a replacement lifecycle implementation.
pub fn original_type(_: Option<awaken_runtime::EmbeddedSession<'_>>) {}
fn main() {}
''',
    'old_runtime_method': r'''// Runtime release R3: the first pre-release source exposes the old method
// -> identical downstream compiles there. R4: candidate removes that method
// -> original E0599 at the call span, independent of the old type name.
// A supplied context is only typechecked; main starts no Run/model/tool.
pub fn original_entry(
    runtime: &awaken_runtime::Runtime,
    snapshot: &awaken_runtime::ExecutableAgentSnapshot,
    context: awaken_runtime::RuntimeRunContext,
) {
    let _ = runtime.session(snapshot, "same-thread", context);
}
fn main() {}
''',
    'migrated_runtime': r'''// Runtime release R5: current source exposes the canonical binding and method
// -> the downstream type/constructor signature compiles with identical
// Snapshot/Thread/context roles. No legacy alias, second factory, model IO,
// approval loop or durable Session is created by this migration control.
pub fn canonical_entry<'a>(
    runtime: &'a awaken_runtime::Runtime,
    snapshot: &'a awaken_runtime::ExecutableAgentSnapshot,
    context: awaken_runtime::RuntimeRunContext,
) -> Result<awaken_runtime::Conversation<'a>, awaken_runtime::Error> {
    runtime.conversation(snapshot, "same-thread", context)
}
fn main() {}
''',
    'old_retry_policy_decide': r'''// RuntimeContract source-break C/E design, compile-only downstream.
// C1 baseline50b3 exposes decide(FailureClass, ModelExecutionEvidence, ...).
// C2 current2.0 exposes the distinct ModelReentry argument. R1 baseline + this
// identical old source -> compile; R2 current + this source -> designed E0308
// at the single original argument span. Actual compiler/status remains unknown
// until the existing capture owner runs it; no SemVer lint is predicted here.
// Runtime reexports the same RetryPolicy, so the annotation also preserves that
// public type path. No model, credential, filesystem or physical effect runs.
use awaken_runtime_contract::model_operation::ModelExecutionEvidence;
use awaken_runtime_contract::resilience::{FailureClass, RetryPolicy};
use std::time::Duration;

fn main() {
    let policy: awaken_runtime::RetryPolicy = RetryPolicy::default();
    let _ = policy.decide(
        FailureClass::Transient,
        ModelExecutionEvidence::Unknown,
        0,
        Duration::from_secs(60),
        0,
    );
}
''',
    'migrated_retry_policy_decide': r'''// RuntimeContract migration R3: the downstream consumes the existing canonical
// projection of original phase/scope/execution, then calls the same retry law.
// This input is designed to compile on current2.0; actual compiler evidence is
// pending. It does not upgrade Unknown into a terminal receipt, recreate the
// classifier, or add a production wrapper/alias. Existing model-reentry runtime
// tests own its semantic partitions; this is only a downstream type control.
use awaken_runtime_contract::model_operation::{
    ModelEffectScope, ModelExecutionEvidence, ModelOperationPhase, ModelReentry,
};
use awaken_runtime_contract::resilience::{FailureClass, RetryPolicy};
use std::time::Duration;

fn main() {
    let policy: awaken_runtime::RetryPolicy = RetryPolicy::default();
    let reentry = ModelReentry::observed(
        ModelOperationPhase::Entered,
        ModelEffectScope::Opaque,
        ModelExecutionEvidence::Unknown,
    );
    let _ = policy.decide(
        FailureClass::Transient,
        reentry,
        0,
        Duration::from_secs(60),
        0,
    );
}
''',
    'owner_legacy_inventory': r'''// W38 I1: every reviewed old Contract forwarding route was a real baseline promise.
// Baseline-only positive import inventory; API/raw SemVer own the full removal audit.
// This source does not run a model, store, tool, permission or network operation.
use awaken_runtime_contract::activation::InheritedToolsets as Legacy0;
use awaken_runtime_contract::agent_bindings::ToolExecutionPolicy as Legacy1;
use awaken_runtime_contract::agent_bindings::ToolPermissionRequirement as Legacy2;
use awaken_runtime_contract::agent_bindings::ToolPolicyOverride as Legacy3;
use awaken_runtime_contract::agent_bindings::ToolsetPolicy as Legacy4;
use awaken_runtime_contract::agent_bindings::ToolsetSource as Legacy5;
use awaken_runtime_contract::credential::CandidateFingerprint as Legacy6;
use awaken_runtime_contract::credential::CredentialAccess as Legacy7;
use awaken_runtime_contract::credential::CredentialAudienceRef as Legacy8;
use awaken_runtime_contract::credential::CredentialEnvelope as Legacy9;
use awaken_runtime_contract::credential::CredentialExecutionPolicy as Legacy10;
use awaken_runtime_contract::credential::CredentialMaterialSource as Legacy11;
use awaken_runtime_contract::credential::CredentialPurpose as Legacy12;
use awaken_runtime_contract::credential::CredentialRef as Legacy13;
use awaken_runtime_contract::credential::CredentialRefreshAccess as Legacy14;
use awaken_runtime_contract::credential::CredentialTarget as Legacy15;
use awaken_runtime_contract::credential::CredentialUsage as Legacy16;
use awaken_runtime_contract::credential::CredentialUsageError as Legacy17;
use awaken_runtime_contract::credential::HttpEffectPlacement as Legacy18;
use awaken_runtime_contract::credential::ModelExposurePolicy as Legacy19;
use awaken_runtime_contract::credential::PRIVATE_FILE_CONSUMER as Legacy20;
use awaken_runtime_contract::credential::PRIVATE_FILE_MATERIAL_TYPE as Legacy21;
use awaken_runtime_contract::credential::PROCESS_ENV_CONSUMER as Legacy22;
use awaken_runtime_contract::credential::PROCESS_ENV_MATERIAL_TYPE as Legacy23;
use awaken_runtime_contract::credential::PlaintextBoundary as Legacy24;
use awaken_runtime_contract::credential::PlaintextHolder as Legacy25;
use awaken_runtime_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN as Legacy26;
use awaken_runtime_contract::credential::SELF_HOSTED_SANDBOX_TRUST_DOMAIN as Legacy27;
use awaken_runtime_contract::credential::SELF_HOSTED_WORKER_TRUST_DOMAIN as Legacy28;
use awaken_runtime_contract::credential::SealedCredentialEnvelopeRef as Legacy29;
use awaken_runtime_contract::credential::SignatureVerificationAlgorithm as Legacy30;
use awaken_runtime_contract::credential::TokenEndpointAuth as Legacy31;
use awaken_runtime_contract::credential::TrustDomainRef as Legacy32;
use awaken_runtime_contract::credential::is_core_delivery_consumer as Legacy33;
use awaken_runtime_contract::delegation::ChildRunCancellation as Legacy34;
use awaken_runtime_contract::delegation::DelegationLimits as Legacy35;
use awaken_runtime_contract::tool as Legacy36;
use awaken_runtime_contract::tool_batch as Legacy37;
use awaken_runtime_contract::tool_discovery as Legacy38;
use awaken_runtime_contract::CandidateFingerprint as Legacy39;
use awaken_runtime_contract::CredentialAccess as Legacy40;
use awaken_runtime_contract::CredentialEnvelope as Legacy41;
use awaken_runtime_contract::CredentialExecutionPolicy as Legacy42;
use awaken_runtime_contract::CredentialMaterialSource as Legacy43;
use awaken_runtime_contract::CredentialRef as Legacy44;
use awaken_runtime_contract::CredentialRefreshAccess as Legacy45;
use awaken_runtime_contract::CredentialTarget as Legacy46;
use awaken_runtime_contract::CredentialUsage as Legacy47;
use awaken_runtime_contract::CredentialUsageError as Legacy48;
use awaken_runtime_contract::HttpEffectPlacement as Legacy49;
use awaken_runtime_contract::ModelExposurePolicy as Legacy50;
use awaken_runtime_contract::PlaintextBoundary as Legacy51;
use awaken_runtime_contract::PlaintextHolder as Legacy52;
use awaken_runtime_contract::SealedCredentialEnvelopeRef as Legacy53;
use awaken_runtime_contract::TokenEndpointAuth as Legacy54;
use awaken_runtime_contract::TrustDomainRef as Legacy55;
use awaken_runtime_contract::ChildRunCancellation as Legacy56;
use awaken_runtime_contract::DelegationLimits as Legacy57;
use awaken_runtime_contract::ResumeTicket as Legacy58;
use awaken_runtime_contract::ContentBlock as Legacy59;
use awaken_runtime_contract::ImageSource as Legacy60;
use awaken_runtime_contract::extract_text as Legacy61;
use awaken_runtime_contract::MessageId as Legacy62;
use awaken_runtime_contract::Message as Legacy63;
use awaken_runtime_contract::Role as Legacy64;
use awaken_runtime_contract::EndCause as Legacy65;
use awaken_runtime_contract::RunId as Legacy66;
use awaken_runtime_contract::RunRecord as Legacy67;
use awaken_runtime_contract::RunState as Legacy68;
use awaken_runtime_contract::StateCommand as Legacy69;
use awaken_runtime_contract::StateKey as Legacy70;
use awaken_runtime_contract::StateCell as Legacy71;
use awaken_runtime_contract::MergePolicy as Legacy72;
use awaken_runtime_contract::Scope as Legacy73;
use awaken_runtime_contract::Store as Legacy74;
use awaken_runtime_contract::ThreadId as Legacy75;
use awaken_runtime_contract::RedactedString as Legacy76;
use awaken_runtime_contract::CommitCoordinator as Legacy77;
use awaken_runtime_contract::CommitError as Legacy78;
use awaken_runtime_contract::CommitOperationCoordinator as Legacy79;
use awaken_runtime_contract::CommitHashError as Legacy80;
use awaken_runtime_contract::CommitOperation as Legacy81;
use awaken_runtime_contract::CommitOperationId as Legacy82;
use awaken_runtime_contract::CommitPayloadHash as Legacy83;
use awaken_runtime_contract::CommitReceipt as Legacy84;
use awaken_runtime_contract::commit_payload_hash as Legacy85;
use awaken_runtime_contract::CommitRecord as Legacy86;
use awaken_runtime_contract::RunDisposition as Legacy87;
use awaken_runtime_contract::ThreadCommit as Legacy88;
use awaken_runtime_contract::CommittedThreadView as Legacy89;
use awaken_runtime_contract::RecoveryError as Legacy90;
use awaken_runtime_contract::RunRecoverySnapshot as Legacy91;
use awaken_runtime_contract::RunRecoverySource as Legacy92;
use awaken_runtime_contract::TranscriptRange as Legacy93;
use awaken_runtime_contract::TranscriptSliceSpec as Legacy94;
use awaken_runtime_contract::TranscriptSnapshotRef as Legacy95;
use awaken_runtime_contract::TranscriptView as Legacy96;
use awaken_runtime_contract::ExecutionScopeRef as Legacy97;
use awaken_runtime_contract::content_fingerprint as Legacy98;
use awaken_runtime_contract::RawTool as Legacy99;
use awaken_runtime_contract::RawToolRegistry as Legacy100;
use awaken_runtime_contract::Tool as Legacy101;
use awaken_runtime_contract::ToolConcurrency as Legacy102;
use awaken_runtime_contract::ToolExecutionAdmission as Legacy103;
use awaken_runtime_contract::ToolExecutionPermit as Legacy104;
use awaken_runtime_contract::ToolExecutionTarget as Legacy105;
use awaken_runtime_contract::ToolExecutionWave as Legacy106;
use awaken_runtime_contract::ToolExecutor as Legacy107;
use awaken_runtime_contract::ToolOperationToken as Legacy108;
use awaken_runtime_contract::ToolOutput as Legacy109;
use awaken_runtime_contract::ToolRecoveryCapability as Legacy110;
use awaken_runtime_contract::ToolRecoveryMode as Legacy111;
use awaken_runtime_contract::ToolRecoveryPolicy as Legacy112;
use awaken_runtime_contract::ToolResource as Legacy113;
use awaken_runtime_contract::ToolResourceAccess as Legacy114;
use awaken_runtime_contract::ToolTaskHandle as Legacy115;
use awaken_runtime_contract::ToolTaskPoll as Legacy116;
use awaken_runtime_contract::ToolTaskStart as Legacy117;
use awaken_runtime_contract::tool_execution_waves as Legacy118;
use awaken_runtime_contract::ActiveToolBatch as Legacy119;
use awaken_runtime_contract::ToolBatch as Legacy120;
use awaken_runtime_contract::ToolBatchId as Legacy121;
use awaken_runtime_contract::ToolBatchPhase as Legacy122;
use awaken_runtime_contract::ToolCallPhase as Legacy123;
use awaken_runtime_contract::ToolWait as Legacy124;
use awaken_runtime_contract::ToolWaitKind as Legacy125;
use awaken_runtime_contract::ToolSearchInput as Legacy126;
use awaken_runtime_contract::ToolSearchLimit as Legacy127;
use awaken_runtime_contract::ToolSearchLimitError as Legacy128;
use awaken_runtime_contract::llm::ModelRequestObservation as Legacy129;
use awaken_runtime_contract::llm::TokenUsage as Legacy130;
use awaken_runtime_contract::llm::ToolCall as Legacy131;
use awaken_runtime_contract::permission::ToolCall as Legacy132;
use awaken_runtime_contract::permission::PermissionDecider as Legacy133;
use awaken_runtime_contract::permission::PermissionPolicy as Legacy134;
use awaken_runtime_contract::permission::PermissionReasonCode as Legacy135;
use awaken_runtime_contract::resilience::Classify as Legacy136;
use awaken_runtime_contract::resilience::Disposition as Legacy137;
use awaken_runtime_contract::resolution::content_fingerprint as Legacy138;
use awaken_runtime_contract::resolved::AcpSessionConfiguration as Legacy139;
use awaken_runtime_contract::resume::PermissionDecision as Legacy140;
fn main() {}
''',
    'owner_old_message_id': r'''// W38 R1-R5: the exact baseline public route compiles on old source; after
// removing redundant Contract exports, the same isolated route must refuse.
// Expected compiler code is design data, not an observed result or SemVer finding.
pub type Legacy = Option<awaken_runtime_contract::MessageId>;
fn main() {}
''',
    'owner_old_credential_module': r'''// W38 R1-R5: the exact baseline public route compiles on old source; after
// removing redundant Contract exports, the same isolated route must refuse.
// Expected compiler code is design data, not an observed result or SemVer finding.
pub type Legacy = Option<awaken_runtime_contract::credential::CredentialRef>;
fn main() {}
''',
    'owner_old_credential_root': r'''// W38 R1-R5: the exact baseline public route compiles on old source; after
// removing redundant Contract exports, the same isolated route must refuse.
// Expected compiler code is design data, not an observed result or SemVer finding.
pub type Legacy = Option<awaken_runtime_contract::CredentialRef>;
fn main() {}
''',
    'owner_old_tool_module': r'''// W38 R1-R5: the exact baseline public route compiles on old source; after
// removing redundant Contract exports, the same isolated route must refuse.
// Expected compiler code is design data, not an observed result or SemVer finding.
pub type Legacy = Option<awaken_runtime_contract::tool::ToolOutput>;
fn main() {}
''',
    'owner_old_tool_root': r'''// W38 R1-R5: the exact baseline public route compiles on old source; after
// removing redundant Contract exports, the same isolated route must refuse.
// Expected compiler code is design data, not an observed result or SemVer finding.
pub type Legacy = Option<awaken_runtime_contract::ToolOutput>;
fn main() {}
''',
    'owner_canonical': r'''// W38 R6: direct existing Agent/Tool owners preserve distinct coordinates and
// original credential/tool types on both baseline and migrated current source.
// Real versions/source/features are bound by the existing metadata validator.
// No value reconstruction, adapter, alias compatibility path or execution occurs.
pub fn canonical(
    message: awaken_agent_contract::agent::message::Id,
    run: awaken_agent_contract::agent::run::Id,
    thread: awaken_agent_contract::agent::thread::Id,
    credential: awaken_agent_contract::credential::CredentialRef,
    call: awaken_tool_contract::tool::ToolCall,
) {
    let _ = (message, run, thread, credential, call);
}
// R7: existing Runtime facade names remain the same original Tool type.
// Independent return types reject an accidental private path or new wrapper.
pub fn model_facade(value: awaken_runtime::model::ToolCall) -> awaken_tool_contract::tool::ToolCall {
    value
}
pub fn permission_facade(value: awaken_runtime::permission::ToolCall) -> awaken_tool_contract::tool::ToolCall {
    value
}
fn main() {}
''',
    'inference_old_disposition': r'''// Inference public-source C/E I1: baseline50b3's exhaustive public Agent enum
// has exactly Transient/Quota/Permanent and no non_exhaustive attribute.
// Identical downstream -> baseline positive; new StopRouting -> designed E0004
// at this match. Actual rustc exit/code/span is pending the original capture.
// Canonical Agent path on both sides isolates this cause from W38 route removal.
use awaken_agent_contract::resilience::Disposition;

pub fn classify(disposition: Disposition) -> u8 {
    match disposition {
        Disposition::Transient => 0,
        Disposition::Quota { .. } => 1,
        Disposition::Permanent => 2,
    }
}
fn main() {}
''',
    'inference_migrated_disposition': r'''// Inference I2: current Agent adds StopRouting for an application-owned routing
// refusal. An explicit arm consumes that new value; this is a type control,
// not a second router/classifier or a claim that all routing outcomes coincide.
// Designed positive only; real compiler evidence is still pending.
use awaken_agent_contract::resilience::Disposition;

pub fn classify(disposition: Disposition) -> u8 {
    match disposition {
        Disposition::Transient => 0,
        Disposition::Quota { .. } => 1,
        Disposition::Permanent => 2,
        Disposition::StopRouting => 3,
    }
}
fn main() {}
''',
    'inference_old_error': r'''// Inference I3: baseline50b3 exposes these fifteen llm::Error variants with
// exhaustive matching allowed. Identical old source -> baseline positive;
// current Admission addition -> designed E0004 at this match, independently
// of ModelRequestGate and W38. Real compiler diagnostics remain unobserved.
use awaken_runtime_contract::llm::Error;

pub fn classify(error: Error) -> u8 {
    match error {
        Error::Observed { .. } => 0,
        Error::Binding(_) => 1,
        Error::Provider(_) => 2,
        Error::InvalidResponse(_) => 3,
        Error::RateLimited { .. } => 4,
        Error::Overloaded { .. } => 5,
        Error::Timeout(_) => 6,
        Error::ContextOverflow(_) => 7,
        Error::InvalidRequest(_) => 8,
        Error::Unauthorized(_) => 9,
        Error::UsageLimit { .. } => 10,
        Error::LoginRequired(_) => 11,
        Error::ModelNotFound(_) => 12,
        Error::ContentFiltered(_) => 13,
        Error::Cancelled(_) => 14,
    }
}
fn main() {}
''',
    'inference_migrated_error': r'''// Inference I4: explicitly handle current Admission without flattening its
// nested cause or inventing a new classification rule. This compile-only
// shape control does not execute or approve retry, failover or re-entry.
// Designed positive only; actual compiler capture remains required.
use awaken_runtime_contract::llm::Error;

pub fn classify(error: Error) -> u8 {
    match error {
        Error::Observed { .. } => 0,
        Error::Binding(_) => 1,
        Error::Provider(_) => 2,
        Error::InvalidResponse(_) => 3,
        Error::RateLimited { .. } => 4,
        Error::Overloaded { .. } => 5,
        Error::Timeout(_) => 6,
        Error::ContextOverflow(_) => 7,
        Error::InvalidRequest(_) => 8,
        Error::Unauthorized(_) => 9,
        Error::UsageLimit { .. } => 10,
        Error::LoginRequired(_) => 11,
        Error::ModelNotFound(_) => 12,
        Error::ContentFiltered(_) => 13,
        Error::Cancelled(_) => 14,
        Error::Admission { .. } => 15,
    }
}
fn main() {}
''',
    'inference_old_gate': r'''// Inference I5: baseline50b3's existing object-safe gate returns String errors.
// The same plain async caller -> baseline positive; current typed llm::Error
// -> designed E0308 at the awaited expression. No async-trait dependency or
// adapter implementation is added. Actual status/code/span remains pending.
use awaken_runtime_contract::llm::{
    ModelRequestAdmission, ModelRequestAdmissionRequest, ModelRequestGate,
};

pub async fn original_gate(
    gate: &dyn ModelRequestGate,
    request: ModelRequestAdmissionRequest,
) -> Result<ModelRequestAdmission, String> {
    gate.admit_model_request(request).await
}
fn main() {}
''',
    'inference_migrated_gate': r'''// Inference I6: current caller preserves the owning gate's typed error result.
// This only typechecks the original port; it neither invokes a model nor adds
// a gate, classifier or error conversion. Designed positive, actual pending.
use awaken_runtime_contract::llm::{
    ModelRequestAdmission, ModelRequestAdmissionRequest, ModelRequestGate,
};

pub async fn canonical_gate(
    gate: &dyn ModelRequestGate,
    request: ModelRequestAdmissionRequest,
) -> awaken_runtime_contract::llm::Result<ModelRequestAdmission> {
    gate.admit_model_request(request).await
}
fn main() {}
''',
    'inference_old_runtime_ceiling': r'''// Inference I7: baseline50b3 publicly exposes the consecutive-failed-step setter.
// Identical downstream -> baseline positive; current deletion -> designed
// E0599 at this independent method call. It does not mention any removed type
// alias/private getter. Real rustc evidence remains pending; main runs no IO.
pub fn original_ceiling(runtime: awaken_runtime::Runtime) -> awaken_runtime::Runtime {
    runtime.with_max_consecutive_inference_failures(3)
}
fn main() {}
''',
    'inference_migrated_runtime_policy': r'''// Inference I8: current Runtime retains the authoritative per-request retry
// policy entry. Typecheck that existing API with a supplied policy. This does
// NOT equate the old failed-step ceiling of 3 with request retry attempts;
// removing the parallel budget intentionally has no such numeric mapping.
// Designed positive only; actual compiler evidence is pending.
pub fn canonical_policy(
    runtime: awaken_runtime::Runtime,
    policy: awaken_runtime_contract::resilience::RetryPolicy,
) -> awaken_runtime::Runtime {
    runtime.with_retry_policy(policy)
}
fn main() {}
''',
    'mcp_old_transport_error': r'''// Compiler C/E: baseline exhaustive twelve-variant match -> build; the same
// source against current ToolFault -> E0004 at the original match; explicit
// ToolFault handling -> current build. No wildcard, shim, local error copy or
// production compatibility path may hide the source break (ADR-0103).
use awaken_mcp_wire::McpTransportError;

pub fn classify(error: McpTransportError) -> bool {
    match error {
        McpTransportError::NotDispatched(_)
        | McpTransportError::UnknownTool(_)
        | McpTransportError::ServerNotFound(_)
        | McpTransportError::ServerError(_)
        | McpTransportError::TransportError(_)
        | McpTransportError::IoError(_)
        | McpTransportError::JsonError(_)
        | McpTransportError::Timeout(_)
        | McpTransportError::ProtocolError(_)
        | McpTransportError::NotSupported(_)
        | McpTransportError::ConnectionClosed
        | McpTransportError::ServerRestarting(_) => false,
    }
}

fn main() {
    let _classifier: fn(McpTransportError) -> bool = classify;
}
''',
    'mcp_migrated_transport_error': r'''// Compiler C/E: baseline exhaustive twelve-variant match -> build; the same
// source against current ToolFault -> E0004 at the original match; explicit
// ToolFault handling -> current build. No wildcard, shim, local error copy or
// production compatibility path may hide the source break (ADR-0103).
use awaken_mcp_wire::McpTransportError;

pub fn classify(error: McpTransportError) -> bool {
    match error {
        McpTransportError::NotDispatched(_)
        | McpTransportError::UnknownTool(_)
        | McpTransportError::ServerNotFound(_)
        | McpTransportError::ServerError(_)
        | McpTransportError::TransportError(_)
        | McpTransportError::IoError(_)
        | McpTransportError::JsonError(_)
        | McpTransportError::Timeout(_)
        | McpTransportError::ProtocolError(_)
        | McpTransportError::NotSupported(_)
        | McpTransportError::ConnectionClosed
        | McpTransportError::ServerRestarting(_) => false,
        McpTransportError::ToolFault(_, _) => true,
    }
}

fn main() {
    let _classifier: fn(McpTransportError) -> bool = classify;
}
''',
}
