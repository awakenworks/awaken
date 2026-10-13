#!/usr/bin/env python3
"""Read ``scripts/ci/gate_tools.json``: the one declaration of what the gates need.

Usage:
  gate_tools.py report                     the whole machine, tool by tool
  gate_tools.py gates                      every gate name the manifest uses
  gate_tools.py gates-of <tool>            the gates one tool gates
  gate_tools.py rows                       one `name|gates|install` line per tool
  gate_tools.py locate <tool>              print where it is, or fail
  gate_tools.py require --gate <gate>      fail when a required tool is absent
  gate_tools.py solver-status <solver>     decide one Kani solver before it runs
  gate_tools.py disk --stage <stage>       fail when the target filesystem is small
  gate_tools.py --self-test                prove the manifest against this machine

Why this exists
---------------
Four separate gate failures in one session named the wrong cause. The pinned
solver was not on PATH, so Kani reported "0 of N failed ... VERIFICATION
FAILED" — a proof regression. ``kissat`` was not on PATH, so the driver reported
``execvp kissat failed``. ``e2e/node_modules`` was not installed, so a runner
reported ``ERR_MODULE_NOT_FOUND`` — a broken import. The object-store image was
not pulled, so Docker reported "pull access denied" — a permission problem. None
of those messages named a missing tool, and each hid the next.

The fix is structural: every external thing a gate needs is declared once, in
data, with the method that locates it; a stage checks what it needs *before* it
runs; and the self-test compares the declaration against this machine, so a row
that no longer describes reality fails here instead of inside a proof.

This module is the only reader of that manifest. ``doctor.sh`` reports from it,
``check_formal.sh`` decides solvers through it, and ``check-rust.sh`` and
``pg_tests.sh`` precheck through it.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

# Input set: the manifest, the checker that owns Node roots, the script that
# owns the appended tool homes, and the group list this manifest's gate names
# must stay inside. A change to any of them can make a row wrong.
INPUT_GLOBS: tuple[str, ...] = (
    "scripts/ci/gate_tools.json",
    "scripts/ci/check_node_dependencies.py",
    "scripts/ci/_provider_environment.sh",
    "scripts/ci/check-all.sh",
)

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = Path(__file__).resolve().parent / "gate_tools.json"
PROVIDER_ENVIRONMENT = ROOT / "scripts" / "ci" / "_provider_environment.sh"
CHECK_ALL = ROOT / "scripts" / "ci" / "check-all.sh"

LOCATE_KINDS = {
    "path",
    "bundle",
    "kani-bundle",
    "cargo-bin",
    "rust-toolchain",
    "file",
    "container-image",
    "node-root",
    "builtin",
}
REQUIREMENTS = {"required", "optional", "fetched"}

# Exit codes `solver-status` uses, so a shell caller can branch without parsing.
SOLVER_USABLE = 0
SOLVER_ABSENT = 3
SOLVER_REFUSED = 4
HOST_ADMISSION_VERSION = "awaken-build-admission 1 sha256="

# Defaults the repository's own scripts already apply, so an unset variable
# resolves to the same directory here as it does there.
XDG_DEFAULTS = {
    "XDG_DATA_HOME": "{home}/.local/share",
    "XDG_BIN_HOME": "{home}/.local/bin",
}


def load() -> dict:
    return json.loads(MANIFEST.read_text(encoding="utf-8"))


def expand(text: str) -> str:
    """``$HOME`` and the XDG homes, resolved the way the shell scripts resolve them."""

    home = os.environ.get("HOME", str(Path.home()))
    resolved = text.replace("$HOME", home).replace("${HOME}", home)
    for name, default in XDG_DEFAULTS.items():
        value = os.environ.get(name) or default.format(home=home)
        resolved = resolved.replace(f"${{{name}}}", value).replace(f"${name}", value)
    # A name this process does carry (CARGO_TARGET_DIR, say) resolves too; one it
    # does not is left written as it is, because that is still readable advice.
    return os.path.expandvars(resolved)


class Found:
    """Where one declared tool is, or why it is not here — in three outcomes.

    "absent" and "undecidable" are different answers and a gate must not confuse
    them. Absent is a fact about the machine: the binary is not on PATH, the
    image is not cached, the toolchain is not installed. Undecidable is a fact
    about the *probe*: a stopped Docker daemon, a `rustup` that could not list,
    a query that could not run. Collapsing the second into the first is how a
    stopped daemon read as "image absent", and an optional row that reads absent
    turns into a successful skip — a gate reporting green about a substrate it
    never observed. An undecidable probe therefore fails by name instead.
    """

    STATUSES = ("present", "builtin", "absent", "undecidable")

    def __init__(self, status: str, where: str = "", detail: str = "") -> None:
        assert status in self.STATUSES, status
        self.status = status
        self.where = where
        self.detail = detail

    @property
    def usable(self) -> bool:
        return self.status in ("present", "builtin")

    @property
    def undecidable(self) -> bool:
        return self.status == "undecidable"


def _on_path(command: str) -> str | None:
    return shutil.which(command)


def _bundle_directories(tool: dict) -> list[str]:
    return [expand(directory) for directory in tool.get("directories", [])]


def _kani_solver_directory() -> tuple[str | None, str | None]:
    """The released Kani bundle's bin directory, from the script that owns it.

    Returns (directory, undecidable_reason). A query that could not run says so;
    it does not report the bundle as absent, because it did not look.
    """

    script = ROOT / "scripts" / "ci" / "bootstrap_kani.sh"
    if not script.is_file():
        return None, None  # the bootstrap is genuinely not in this checkout
    try:
        result = subprocess.run(
            [str(script), "--solver-tools"],
            capture_output=True,
            text=True,
            timeout=120,
            cwd=ROOT,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return None, f"the Kani bootstrap could not be queried for its bundle: {error}"
    if result.returncode != 0:
        return None, (
            "the Kani bootstrap exited "
            f"{result.returncode} instead of naming its bundle directory"
        )
    return (result.stdout.strip().splitlines()[-1] if result.stdout.strip() else None), None


def _node_root_missing(relative: str) -> list[str]:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import check_node_dependencies  # noqa: PLC0415 - deliberately late, it owns this decision

    return check_node_dependencies.dependency_errors(ROOT, (relative,))


# What a container engine says when it looked and the image was not there. Any
# other failure means it did not look: a stopped daemon, a permission denial, a
# socket timeout. Those are undecidable, not absent.
IMAGE_ABSENT_EVIDENCE = (
    "no such image",
    "no such object",
    "manifest unknown",
    "reference does not exist",
    "failed to find image",
    "image not known",
)


def _image_status(reference: str) -> Found:
    engine = _on_path("docker") or _on_path("podman")
    if engine is None:
        return Found(
            "undecidable",
            detail="no container engine is on PATH, so nothing here can say whether "
            f"{reference} is cached; install docker or podman, or declare this gate unrunnable",
        )
    try:
        result = subprocess.run(
            [engine, "image", "inspect", reference],
            capture_output=True,
            text=True,
            timeout=120,
        )
    except subprocess.TimeoutExpired:
        return Found(
            "undecidable",
            detail=f"{engine} image inspect did not answer within 120s; the daemon is not "
            "responding, which is not the same as the image being absent",
        )
    except (OSError, subprocess.SubprocessError) as error:
        return Found("undecidable", detail=f"{engine} image inspect could not run: {error}")
    if result.returncode == 0:
        return Found("present", reference, "cached locally")
    message = (result.stderr or result.stdout or "").lower()
    if any(evidence in message for evidence in IMAGE_ABSENT_EVIDENCE):
        return Found("absent")
    first = (result.stderr or result.stdout or "").strip().splitlines()
    return Found(
        "undecidable",
        detail=f"{engine} image inspect failed without saying the image is absent: "
        + (first[0] if first else f"exit {result.returncode}"),
    )


def locate(tool: dict) -> Found:
    """Find one tool by exactly the method its row states."""

    kind = tool["locate"]
    name = tool["name"]
    override = tool.get("env_override")
    if override and os.environ.get(override):
        candidate = expand(os.environ[override])
        if Path(candidate).exists():
            return Found("present", candidate, f"selected by ${override}")
        return Found("absent", detail=f"${override} names {candidate}, which does not exist")

    if kind == "builtin":
        return Found("builtin", detail=tool.get("install", ""))
    if kind == "path":
        where = _on_path(name)
        return Found("present", where) if where else Found("absent")
    if kind == "bundle":
        where = _on_path(name)
        if where:
            return Found("present", where)
        for directory in _bundle_directories(tool):
            candidate = Path(directory) / name
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return Found("present", str(candidate), "in an appended tool home")
        return Found("absent")
    if kind == "kani-bundle":
        where = _on_path(name)
        if where:
            return Found("present", where)
        directory, undecidable = _kani_solver_directory()
        if undecidable:
            return Found("undecidable", detail=undecidable)
        if directory:
            candidate = Path(directory) / name
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return Found("present", str(candidate), "in the released Kani bundle")
        return Found("absent")
    if kind == "cargo-bin":
        where = _on_path(name)
        return Found("present", where) if where else Found("absent")
    if kind == "rust-toolchain":
        rustup = _on_path("rustup")
        if rustup is None:
            return Found(
                "undecidable",
                detail="rustup itself is not here, so nothing can list installed toolchains; "
                f"whether {name} exists on this machine is unknown, not answered",
            )
        try:
            result = subprocess.run(
                [rustup, "toolchain", "list"], capture_output=True, text=True, timeout=120
            )
        except (OSError, subprocess.SubprocessError) as error:
            return Found("undecidable", detail=f"rustup could not list toolchains: {error}")
        if result.returncode != 0:
            return Found(
                "undecidable",
                detail=f"rustup toolchain list exited {result.returncode}; the listing failed, "
                "which is not evidence that the toolchain is absent",
            )
        for line in result.stdout.splitlines():
            if line.split()[0].startswith(name) if line.split() else False:
                return Found("present", line.split()[0])
        return Found("absent")
    if kind == "file":
        candidate = expand(tool["path"])
        return Found("present", candidate) if Path(candidate).is_file() else Found("absent")
    if kind == "container-image":
        return _image_status(name)
    if kind == "node-root":
        try:
            problems = _node_root_missing(name)
        except Exception as error:  # noqa: BLE001 - any failure here means we did not look
            return Found(
                "undecidable",
                detail=f"the Node dependency checker could not answer for {name}: {error}",
            )
        if problems:
            return Found("absent", detail=problems[0])
        return Found("present", str(ROOT / name) if name != "." else str(ROOT), "installed")
    raise SystemExit(f"gate_tools: {name} declares an unknown locate kind {kind!r}")


def version_problem(tool: dict, found: Found) -> str | None:
    """A drifted version pin, named. ``None`` when the row still describes reality."""

    pin = tool.get("version")
    if pin is None or not found.usable:
        return None
    command = pin.get("command")
    if not command:
        return None
    executable = found.where if found.where and Path(found.where).exists() else command[0]
    try:
        result = subprocess.run(
            [executable, *command[1:]], capture_output=True, text=True, timeout=120
        )
    except (OSError, subprocess.SubprocessError) as error:
        return f"{tool['name']}: its version could not be read ({error})"
    if result.returncode != 0:
        return f"{tool['name']}: its version could not be read (exit code {result.returncode})"
    actual = (result.stdout or result.stderr).strip().splitlines()
    actual_line = actual[0] if actual else ""
    if "expect" in pin and actual_line != pin["expect"]:
        return (
            f"{tool['name']}: expected {pin['expect']!r} but this machine has "
            f"{actual_line!r} (pin owned by {pin.get('owner', MANIFEST.name)})"
        )
    if "contains" in pin and pin["contains"] not in actual_line:
        return (
            f"{tool['name']}: expected a version containing {pin['contains']!r} but this "
            f"machine has {actual_line!r} (pin owned by {pin.get('owner', MANIFEST.name)})"
        )
    return None


def tools_for(manifest: dict, gate: str) -> list[dict]:
    return [tool for tool in manifest["tools"] if gate in tool["gates"]]


def gate_names(manifest: dict) -> list[str]:
    return sorted({gate for tool in manifest["tools"] for gate in tool["gates"]})


def find_tool(manifest: dict, name: str) -> dict | None:
    for tool in manifest["tools"]:
        if tool["name"] == name:
            return tool
    return None


def describe(tool: dict) -> str:
    """The one-line consequence of this tool being absent."""

    if tool["requirement"] == "required":
        return "required"
    if tool["requirement"] == "fetched":
        return "fetched on demand"
    condition = tool.get("condition")
    skip = tool.get("skip", "part of its gate")
    return f"optional: skips {skip}" + (f" ({condition})" if condition else "")


# ---------------------------------------------------------------- commands


def command_report(manifest: dict) -> int:
    absent_required: list[str] = []
    undecided: list[str] = []
    print(f"{'TOOL':<44} {'STATUS':<9} {'HOW IT IS FOUND':<16} GATES / EFFECT")
    for tool in manifest["tools"]:
        found = locate(tool)
        status = {
            "present": "present",
            "builtin": "builtin",
            "absent": "ABSENT",
            "undecidable": "UNDECIDED",
        }[found.status]
        gates = " ".join(tool["gates"])
        print(f"{tool['name']:<44} {status:<9} {tool['locate']:<16} {gates}")
        if found.usable:
            if found.where:
                print(f"{'':<44} {'':<9} -> {found.where}")
            problem = version_problem(tool, found)
            if problem:
                print(f"{'':<44} {'':<9} !! {problem}")
            continue
        print(f"{'':<44} {'':<9} -- {describe(tool)}")
        if found.detail:
            print(f"{'':<44} {'':<9} -- {found.detail}")
        print(f"{'':<44} {'':<9} -> install: {tool['install']}")
        if found.undecidable:
            undecided.append(f"{tool['name']} ({gates})")
        elif tool["requirement"] == "required":
            absent_required.append(f"{tool['name']} ({gates})")
    print()
    if undecided:
        print(f"{len(undecided)} tool(s) could not be decided; their gates must not start:")
        for entry in undecided:
            print(f"  - {entry}")
    if absent_required:
        print(f"{len(absent_required)} required tool(s) absent; their gates cannot run:")
        for entry in absent_required:
            print(f"  - {entry}")
    if not undecided and not absent_required:
        print("every required tool of every gate is present on this machine.")
    return 0


def command_require(manifest: dict, gate: str) -> int:
    if gate not in gate_names(manifest):
        print(f"gate_tools: no gate named {gate!r} in {MANIFEST.name}", file=sys.stderr)
        return 2
    missing: list[str] = []
    drifted: list[str] = []
    undecided: list[str] = []
    notes: list[str] = []
    for tool in tools_for(manifest, gate):
        found = locate(tool)
        if found.usable:
            problem = version_problem(tool, found)
            if problem and tool.get("version", {}).get("mismatch") == "hard-error":
                drifted.append(problem)
            elif problem:
                notes.append(f"warning: {problem}")
            continue
        if found.undecidable:
            # The requirement level does not matter here. "Optional" buys a skip
            # when the tool is known to be absent; it buys nothing when the probe
            # failed, because then nobody knows what would have been skipped.
            undecided.append(
                f"undecidable tool for the {gate} gate: {tool['name']}"
                f"\n    the probe did not answer: {found.detail or 'no detail was recorded'}"
                f"\n    located by: {tool['locate']}"
                f"\n    this is not an absent tool and must not become a skip; fix the probe's"
                f"\n    environment (a stopped daemon, a missing lister) and re-run"
            )
            continue
        if tool["requirement"] == "required":
            detail = f" ({found.detail})" if found.detail else ""
            missing.append(
                f"missing tool for the {gate} gate: {tool['name']}{detail}"
                f"\n    located by: {tool['locate']}"
                f"\n    install:    {tool['install']}"
            )
        elif tool["requirement"] == "fetched":
            notes.append(
                f"note: {tool['name']} is not cached locally; the engine will fetch it "
                f"({tool['install']})"
            )
        else:
            notes.append(
                f"note: {tool['name']} is absent, so {gate} will skip "
                f"{tool.get('skip', tool['name'])}"
            )
    for note in notes:
        print(note)
    for problem in drifted:
        print(f"ERROR: {problem}", file=sys.stderr)
    for problem in undecided:
        print(f"ERROR: {problem}", file=sys.stderr)
    for problem in missing:
        print(f"ERROR: {problem}", file=sys.stderr)
    if missing or drifted or undecided:
        print(
            f"the {gate} gate did not start: scripts/ci/doctor.sh reports every tool",
            file=sys.stderr,
        )
        return 1
    print(f"gate '{gate}': every required tool is present")
    return 0


def command_locate(manifest: dict, name: str) -> int:
    tool = find_tool(manifest, name)
    if tool is None:
        print(f"gate_tools: {name!r} is not declared in {MANIFEST.name}", file=sys.stderr)
        return 2
    found = locate(tool)
    if not found.usable:
        print(f"{name} is not here; install: {tool['install']}", file=sys.stderr)
        return 1
    print(found.where or found.status)
    return 0


def command_solver_status(manifest: dict, name: str) -> int:
    """Decide one Kani solver before a package batch runs.

    Kani does not report an absent solver as an absent tool: every check ends in
    ERROR or ``execvp <solver> failed`` and the batch reads "0 of N failed ...
    VERIFICATION FAILED", which is indistinguishable from a proof regression.
    So the shell asks here first, and the answer is an exit code:

    | Rule | solver row                          | exit           | stdout        |
    |------|-------------------------------------|----------------|---------------|
    | S1   | compiled into the backend           | SOLVER_USABLE  | builtin       |
    | S2   | found by its stated method          | SOLVER_USABLE  | present       |
    | S3   | absent                              | SOLVER_ABSENT  | why, to skip  |
    | S4   | present at a version that is not the pin | SOLVER_REFUSED | -        |
    | S5   | not declared in the manifest at all | SOLVER_REFUSED | -             |
    """

    tool = find_tool(manifest, name)
    if tool is None or not tool.get("solver"):
        print(
            f"solver {name!r} is not declared in {MANIFEST.name}; declare it there "
            "before a harness selects it",
            file=sys.stderr,
        )
        return SOLVER_REFUSED
    found = locate(tool)
    if found.status == "builtin":
        print("builtin")
        return SOLVER_USABLE
    if not found.usable:
        print(tool.get("absent_reason", f"the {name} solver is not on PATH"))
        return SOLVER_ABSENT
    problem = version_problem(tool, found)
    if problem and tool.get("version", {}).get("mismatch") == "hard-error":
        print(problem, file=sys.stderr)
        return SOLVER_REFUSED
    print("present")
    return SOLVER_USABLE


def disk_minimum(manifest: dict, stage: str) -> dict | None:
    for entry in manifest["disk_minimums"]:
        if entry["stage"] == stage:
            return entry
    return None


def target_directory() -> Path:
    """The filesystem a long stage writes to."""

    configured = os.environ.get("CARGO_TARGET_DIR")
    if configured:
        return Path(configured)
    return ROOT / "target"


def free_gigabytes(path: Path) -> float:
    probe = path
    while not probe.exists() and probe != probe.parent:
        probe = probe.parent
    return shutil.disk_usage(probe).free / (1000**3)


def command_disk(manifest: dict, stage: str, path: str | None, override: float | None) -> int:
    """Refuse a long stage the filesystem cannot hold.

    A full disk reaches the formal gate as ``os error 28`` inside one CBMC write,
    which truncates a GOTO binary, which crashes the backend with SIGABRT several
    minutes later. The named cause is checked here instead.
    """

    entry = disk_minimum(manifest, stage)
    if entry is None:
        print(f"gate_tools: no disk minimum declared for stage {stage!r}", file=sys.stderr)
        return 2
    required = float(override) if override is not None else float(entry["gigabytes"])
    target = Path(path) if path else target_directory()
    free = free_gigabytes(target)
    if free >= required:
        print(f"disk: {free:.1f}G free at {target} (stage '{stage}' needs {required:.0f}G)")
        return command_resources(manifest, os.environ.get('AWAKEN_ADMISSION_PROFILE', 'cargo'))
    print(
        f"ERROR: not enough free disk for the {stage} stage: {free:.1f}G free at {target}, "
        f"{required:.0f}G required\n"
        f"    why: {entry['reason']}\n"
        f"    safe to delete:",
        file=sys.stderr,
    )
    for suggestion in manifest["cleanup_suggestions"]:
        print(f"      - {expand(suggestion)}", file=sys.stderr)
    return 75


def command_resources(manifest: dict, profile: str, *, require_host: bool = False,
                      observe: bool = False) -> int:
    """Delegate resource eligibility; the repository owns no host thresholds."""
    tool = find_tool(manifest, 'awaken-build-admission')
    found = locate(tool) if tool else Found('absent')
    if not found.usable or not found.where:
        if require_host:
            print('full gate requires the canonical awaken-build-admission command', file=sys.stderr)
            return 75
        print('host resource observation unavailable; runner must supply its resource contract')
        return 0
    try:
        result = subprocess.run([found.where, '--status' if observe else '--require',
                                 '--profile', profile], capture_output=True, text=True, timeout=30)
        report = json.loads(result.stdout)
        if (type(report.get('schema_version')) is not int or report.get('schema_version') != 1
                or report.get('profile') != profile
                or type(report.get('eligible')) is not bool
                or not isinstance(report.get('problems'), list)
                or result.returncode != (0 if observe or report['eligible'] else 75)):
            raise ValueError('inconsistent host resource observation')
    except (OSError, subprocess.SubprocessError, ValueError, TypeError, AttributeError) as error:
        print(f'host resource qualification unavailable: {error}', file=sys.stderr)
        return 75
    print('host resources: ' + json.dumps(report, sort_keys=True))
    if result.stderr:
        print(result.stderr, file=sys.stderr, end='')
    return result.returncode


def _host_resource_problems() -> list[str]:
    # Resource adapter C/E table: R1 exact protocol/profile+eligible+zero -> run;
    # R2 unavailable+75 -> refusal; R3 status-only may observe R2 before canonical
    # admission tries GC; R4 malformed/inconsistent response -> 75; R5 required
    # host absent -> 75. No percentages/floors or fallback GC are defined here.
    from unittest.mock import patch
    errors = []
    row = {'name': 'awaken-build-admission', 'locate': 'path'}
    fixture = {'tools': [row]}
    for eligible, status, observe, expected in (
            (True, 0, False, 0), (False, 75, False, 75),
            (False, 0, True, 0), (False, 0, False, 75), (True, 75, False, 75)):
        report = {'schema_version': 1, 'profile': 'qualification',
                  'eligible': eligible, 'problems': [] if eligible else ['pressure']}
        with patch(__name__ + '.locate', return_value=Found('present', '/fixture/observer')), \
                patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess(
                    [], status, json.dumps(report), '')) as child:
            code = _quiet(lambda: command_resources(fixture, 'qualification',
                                                    require_host=True, observe=observe))
            if code != expected:
                errors.append('host resource response was classified incorrectly')
            if child.call_args.args[0] != ['/fixture/observer', '--status' if observe else '--require',
                                           '--profile', 'qualification']:
                errors.append('host resource request did not preserve exact profile/mode')
    with patch(__name__ + '.locate', return_value=Found('absent')), \
            patch.object(subprocess, 'run') as child:
        if _quiet(lambda: command_resources(fixture, 'qualification', require_host=True)) != 75:
            errors.append('missing host admitted full gate')
        child.assert_not_called()
    for response in ('not-json', '[]', '{"schema_version":true,"profile":"qualification","eligible":true,"problems":[]}'):
        with patch(__name__ + '.locate', return_value=Found('present', '/fixture/observer')), \
                patch.object(subprocess, 'run', return_value=subprocess.CompletedProcess([], 0, response, '')):
            if _quiet(lambda: command_resources(fixture, 'qualification', require_host=True)) != 75:
                errors.append('invalid resource evidence admitted full gate')
    return errors


# ---------------------------------------------------------------- self-test


def _declared_tool_homes() -> set[str]:
    """Tool homes ``_provider_environment.sh`` appends, read from that script."""

    text = PROVIDER_ENVIRONMENT.read_text(encoding="utf-8")
    body = text.split("awaken_append_repo_tools()", 1)[1].split("\n}", 1)[0]
    homes: set[str] = set()
    for raw in re.findall(r'"(\$\{?[^"]*)"', body):
        candidate = re.sub(r"\$\{([A-Z_]+):-([^}]*)\}", r"${\1}", raw)
        expanded = expand(candidate)
        if expanded.startswith("/"):
            homes.add(expanded.rstrip("/"))
        else:  # `${XDG_BIN_HOME}` with the variable unset resolves through its default
            homes.add(expand(re.sub(r"\$\{?([A-Z_]+)\}?", r"${\1}", raw)).rstrip("/"))
    return homes


def _owner_literal(owner: str) -> tuple[str, str] | None:
    """The literal constant a version pin names, as ``(where, value)``.

    A pin is written twice on purpose: the gate script states it as a shell
    constant its own tests exercise, and the manifest states it so one file
    answers "what does this machine need". Two copies drift, so the manifest's
    ``owner`` field names the constant and this reads it back.
    """

    parts = owner.split()
    if len(parts) != 2:
        return None
    script, variable = parts
    path = ROOT / script
    if not path.is_file():
        return None
    match = re.search(
        rf'^\s*(?:readonly\s+)?{re.escape(variable)}\s*=\s*"([^"]*)"',
        path.read_text(encoding="utf-8"),
        re.MULTILINE,
    )
    return (owner, match.group(1)) if match else None


def _check_all_groups() -> set[str]:
    text = CHECK_ALL.read_text(encoding="utf-8")
    match = re.search(r"^groups=\(([^)]*)\)", text, re.MULTILINE)
    return set(match.group(1).split()) if match else set()


def _quiet(call) -> int:
    """Run one probe for its verdict only; its report belongs to a real caller."""

    import contextlib
    import io

    sink = io.StringIO()
    with contextlib.redirect_stdout(sink), contextlib.redirect_stderr(sink):
        return call()



def _third_outcome_problems() -> list[str]:
    """M10/M11: present, absent and undecidable are three answers, not two.

    Decision table over one container-image row, which is where the collapse was
    live: `_image_present()` returned a bool, so a stopped daemon and an uncached
    image produced the same answer.

    | Rule | engine on PATH | inspect says                    | outcome     | require |
    |------|----------------|---------------------------------|-------------|---------|
    | I1   | yes            | exit 0                          | present     | starts  |
    | I2   | yes            | "No such image"                 | absent      | skips   |
    | I3   | yes            | "Cannot connect to the daemon"  | undecidable | refuses |
    | I4   | no             | (nothing ran)                   | undecidable | refuses |

    I2 is the only row a gate may skip on, and only when the row is optional:
    the engine looked and said no. I3 and I4 refuse whatever the requirement
    level, because nobody knows what the skip would have been skipping.
    """

    import tempfile  # noqa: PLC0415 - only the self-test writes fixtures

    problems: list[str] = []
    original_path = os.environ.get("PATH", "")
    engines = {
        "present": '#!/bin/sh\nexit 0\n',
        "absent": '#!/bin/sh\necho "Error: No such image: fixture:1" >&2\nexit 1\n',
        "undecidable": (
            '#!/bin/sh\necho "Cannot connect to the Docker daemon at unix:///var/run/docker.sock" >&2\n'
            "exit 1\n"
        ),
    }
    row = {
        "name": "fixture:1",
        "locate": "container-image",
        "requirement": "optional",
        "skip": "the fixture substrate",
        "install": "docker pull fixture:1",
        "gates": ["postgres"],
    }
    manifest = {"tools": [row], "disk_minimums": [], "cleanup_suggestions": []}
    with tempfile.TemporaryDirectory() as temporary:
        for expected, body in engines.items():
            home = Path(temporary) / expected
            home.mkdir()
            executable = home / "docker"
            executable.write_text(body, encoding="utf-8")
            executable.chmod(0o755)
            os.environ["PATH"] = str(home)
            try:
                found = locate(row)
                if found.status != expected:
                    problems.append(
                        f"M10 a container engine that reports {expected!r} was read as "
                        f"{found.status!r}"
                    )
                started = _quiet(lambda: command_require(manifest, "postgres")) == 0
                # An optional row may buy a skip only on a decided absence.
                if started != (expected in ("present", "absent")):
                    problems.append(
                        f"M11 an {expected} probe {'started' if started else 'refused'} the gate"
                    )
            finally:
                os.environ["PATH"] = original_path
        empty = Path(temporary) / "empty"
        empty.mkdir()
        os.environ["PATH"] = str(empty)
        try:
            if locate(row).status != "undecidable":
                problems.append("M10 no container engine at all was read as a decided absence")
            if _quiet(lambda: command_require(manifest, "postgres")) == 0:
                problems.append("M11 a gate started with nothing able to answer for its substrate")
        finally:
            os.environ["PATH"] = original_path
    return problems


def self_test() -> int:
    """Prove the manifest against this machine.

    | Rule | cause                                              | effect                         |
    |------|----------------------------------------------------|--------------------------------|
    | M1   | every row is well formed                           | known kind, gates, install     |
    | M2   | an optional row                                    | names what its gate skips      |
    | M3   | every gate name                                    | is a real check-all.sh group   |
    | M4   | every row locates, by its stated method            | present, or absent and skipped |
    | M5   | a located row carries a hard version pin           | this machine matches the pin   |
    | M6   | a `bundle` row names a tool home                   | _provider_environment.sh appends it |
    | M7   | a tiny disk threshold / a huge one                  | accept / refuse by name        |
    | M8   | a solver absent here                               | reports a skip, never a proof failure |
    | M9   | a version pin naming an owning shell constant      | both copies say the same thing |
    | M10  | a probe that answered / that could not answer      | absent / undecidable, never confused |
    | M11  | an undecidable probe                               | refuses the gate, never skips it |
    """

    manifest = load()
    groups = _check_all_groups()
    homes = _declared_tool_homes()
    names: set[str] = set()
    problems: list[str] = []

    for tool in manifest["tools"]:
        name = tool.get("name", "")
        if not name or name in names:
            problems.append(f"M1 duplicate or unnamed row: {name!r}")
        names.add(name)
        if tool.get("locate") not in LOCATE_KINDS:
            problems.append(f"M1 {name}: unknown locate kind {tool.get('locate')!r}")
        if tool.get("requirement") not in REQUIREMENTS:
            problems.append(f"M1 {name}: unknown requirement {tool.get('requirement')!r}")
        if not tool.get("gates"):
            problems.append(f"M1 {name}: names no gate")
        if not tool.get("install"):
            problems.append(f"M1 {name}: says nothing about how to get it")
        if tool.get("requirement") == "optional" and not tool.get("skip"):
            problems.append(f"M2 {name}: optional but does not say what its gate skips")
        if tool.get("requirement") != "optional" and tool.get("skip"):
            problems.append(f"M2 {name}: not optional, so it must not declare a skip")
        for gate in tool.get("gates", []):
            if groups and gate not in groups:
                problems.append(f"M3 {name}: gate {gate!r} is not a check-all.sh group")
        pin = tool.get("version", {})
        if pin.get("owner"):
            literal = _owner_literal(pin["owner"])
            if literal is None:
                problems.append(
                    f"M9 {name}: no constant named by {pin['owner']!r} could be read, so the "
                    "manifest's pin has no owner to agree with"
                )
            else:
                declared = pin.get("expect") or pin.get("contains") or name
                if literal[1] != declared:
                    problems.append(
                        f"M9 {name}: the manifest says {declared!r} but {pin['owner']} says "
                        f"{literal[1]!r}; one pin drifted from the other"
                    )
        if tool.get("locate") == "bundle":
            for directory in _bundle_directories(tool):
                if directory.rstrip("/") not in homes:
                    problems.append(
                        f"M6 {name}: tool home {directory} is not appended by "
                        "_provider_environment.sh, so a non-login shell cannot find it"
                    )

        found = locate(tool)
        if found.usable:
            pin = tool.get("version", {})
            problem = version_problem(tool, found)
            if problem and pin.get("mismatch") == "hard-error":
                problems.append(f"M5 {problem}")
        elif tool["requirement"] == "required":
            # M4: legitimately absent. The report must then name the gate it
            # blocks, so nothing silently runs a gate this machine cannot run.
            for gate in tool["gates"]:
                if _quiet(lambda gate=gate: command_require(manifest, gate)) == 0:
                    problems.append(
                        f"M4 {name} is absent but gate {gate!r} still reports itself ready"
                    )

    for group in sorted(groups - {gate for tool in manifest["tools"] for gate in tool["gates"]}):
        problems.append(
            f"M3 check-all.sh group {group!r} declares no tool, so `require --gate {group}` "
            "cannot answer whether this machine can run it"
        )

    # M5 version certainty C/E: a pin and usable location select one query;
    # its exit and first stdout-or-stderr line determine evidence. A query must
    # succeed before text can certify the pin; failed queries are not absence.
    # | Rule | query status | line                           | helper effect |
    # | V1   | 0            | expect/contains match          | None          |
    # | V2   | 0            | wrong/empty                    | old mismatch  |
    # | V3   | 23/75/-15    | match/wrong/empty, either pipe | named unread  |
    # | V4   | spawn/timeout| no terminal output             | named unread  |
    # | V5   | no pin/command or unusable location           | zero queries  |
    # V1 also retains stdout precedence over stderr. V2/V3/V4 are crossed with
    # required/optional x hard-error/warning: require and solver retain hard
    # refusal versus warning; report remains diagnostic0 but shows the problem.
    # V6 known absent optional/required -> require0/1; undecidable -> require1;
    # solver absent/undecidable/builtin retains3/3/0. No query can redefine Found.
    # These use the existing subprocess IO edge, never a fake Cargo/build tool.
    from unittest.mock import patch
    fixture_tool = {
        "name": "fixture-version", "locate": "path", "gates": ["api"],
        "requirement": "required", "install": "original IO fixture",
        "solver": True,
    }
    matching = "fixture version 1"
    present = Found("present")
    for field in ("expect", "contains"):
        pin = {"command": ["fixture-version", "--version"],
               field: matching if field == "expect" else "version 1"}
        tool = {**fixture_tool, "version": pin}
        observations = [
            (f"{status}/{output}", status, output,
             subprocess.CompletedProcess([], status, stdout, stderr), None)
            for status in (0, 23, 75, -15)
            for output, stdout, stderr in (
                ("stdout", matching + "\n", ""),
                ("stderr", "", matching + "\n"),
                ("wrong", "fixture version 2\n", ""),
                ("empty", "", ""),
                ("precedence", matching + "\n", "fixture version 2\n"),
            )
        ]
        observations.extend(("V4/" + type(error).__name__, None, None, None, error)
                            for error in (OSError("fixture version spawn refusal"),
                                          subprocess.TimeoutExpired(["fixture-version", "--version"], 120)))
        for cause, status, output, result, error in observations:
            rule = f"M5 {field}/{cause}"
            with patch.object(subprocess, "run", return_value=result, side_effect=error) as query:
                problem = version_problem(tool, present)
                query.assert_called_once_with(
                    ["fixture-version", "--version"], capture_output=True,
                    text=True, timeout=120,
                )
            if error is not None:
                if problem != f"fixture-version: its version could not be read ({error})":
                    problems.append(f"{rule}: original spawn/timeout refusal changed")
            elif status:
                expected = f"fixture-version: its version could not be read (exit code {status})"
                if problem != expected:
                    problems.append(f"{rule}: failed query certified text or lost its named status")
            elif output in ("stdout", "stderr", "precedence"):
                if problem is not None:
                    problems.append(f"{rule}: a successful matching version was rejected")
            elif problem is None or "expected" not in problem:
                problems.append(f"{rule}: successful mismatching text lost its old refusal")
            for requirement in ("required", "optional"):
                for policy in ("hard-error", "warning"):
                    judged = {**tool, "requirement": requirement,
                              "version": {**pin, "mismatch": policy}}
                    fixture = {"tools": [judged]}
                    refusal = bool(problem) and policy == "hard-error"
                    with patch(__name__ + ".locate", return_value=present), \
                            patch.object(subprocess, "run", return_value=result, side_effect=error), \
                            patch("builtins.print") as printed:
                        for label, judge, arguments, expected_code in (
                            ("require", command_require, (fixture, "api"), 1 if refusal else 0),
                            ("solver", command_solver_status, (fixture, judged["name"]),
                             SOLVER_REFUSED if refusal else SOLVER_USABLE),
                            ("report", command_report, (fixture,), 0),
                        ):
                            printed.reset_mock()
                            code = judge(*arguments)
                            if code != expected_code:
                                problems.append(f"{rule}/{requirement}/{policy}/{label}: policy changed")
                            output_text = "\n".join(str(value) for call in printed.call_args_list
                                                    for value in call.args)
                            if problem and label in ("require", "report") and problem not in output_text:
                                problems.append(f"{rule}/{label}: diagnostic lost the version problem")
                            if problem and label == "solver" and refusal and problem not in output_text:
                                problems.append(f"{rule}/solver: hard refusal lost the version problem")

    for tool, found in (
        (fixture_tool, present),
        ({**fixture_tool, "version": {"expect": matching}}, present),
        ({**fixture_tool, "version": {"command": ["fixture-version", "--version"]}}, Found("absent")),
        ({**fixture_tool, "version": {"command": ["fixture-version", "--version"]}}, Found("undecidable")),
    ):
        with patch.object(subprocess, "run") as query:
            if version_problem(tool, found) is not None:
                problems.append("M5 V5: an unselected query changed the version result")
            query.assert_not_called()
    for requirement in ("required", "optional"):
        tool = {**fixture_tool, "requirement": requirement,
                "version": {"command": ["fixture-version", "--version"],
                            "expect": matching, "mismatch": "hard-error"}}
        for status in ("absent", "undecidable"):
            with patch(__name__ + ".locate", return_value=Found(status)), \
                    patch.object(subprocess, "run") as query:
                expected_code = 0 if status == "absent" and requirement == "optional" else 1
                if _quiet(lambda: command_require({"tools": [tool]}, "api")) != expected_code:
                    problems.append(f"M5 V6/{requirement}/{status}: existing tool admission changed")
                if _quiet(lambda: command_solver_status({"tools": [tool]}, tool["name"])) != SOLVER_ABSENT:
                    problems.append(f"M5 V6/{status}: existing solver absence changed")
                query.assert_not_called()
    with patch(__name__ + ".locate", return_value=Found("builtin")), \
            patch.object(subprocess, "run") as query:
        if _quiet(lambda: command_solver_status({"tools": [tool]}, tool["name"])) != SOLVER_USABLE:
            problems.append("M5 V6: builtin solver admission changed")
        query.assert_not_called()

    stages = [entry["stage"] for entry in manifest["disk_minimums"]]
    if len(stages) != len(set(stages)):
        problems.append("M1 two disk minimums claim the same stage")
    for entry in manifest["disk_minimums"]:
        if not isinstance(entry.get("gigabytes"), int) or entry["gigabytes"] <= 0:
            problems.append(f"M1 disk minimum for {entry.get('stage')!r} is not a positive size")
        if not entry.get("reason"):
            problems.append(f"M1 disk minimum for {entry.get('stage')!r} gives no reason")
    if not manifest.get("cleanup_suggestions"):
        problems.append("M1 a disk refusal would name nothing that is safe to delete")

    # M7 artifact capacity >=/< stage minimum => delegate once / return 75
    # before the host query. Ambient pressure is a separate cause owned by the
    # resource adapter table; it cannot impersonate an artifact-policy failure.
    with patch(__name__ + '.free_gigabytes', return_value=100), \
            patch(__name__ + '.command_resources', return_value=0) as resources:
        for required in (0.000001, 100):
            resources.reset_mock()
            code = _quiet(lambda: command_disk(manifest, stages[0], None, required))
            if code != 0 or resources.call_count != 1:
                problems.append("M7 sufficient artifact capacity did not delegate to host qualification")
        for required in (100.000001, 10**9):
            resources.reset_mock()
            code = _quiet(lambda: command_disk(manifest, stages[0], None, required))
            if code != 75 or resources.called:
                problems.append("M7 insufficient artifact capacity lost resource refusal or queried the host")

    for tool in manifest["tools"]:
        if not tool.get("solver"):
            continue
        status = _quiet(lambda tool=tool: command_solver_status(manifest, tool["name"]))
        if status not in (SOLVER_USABLE, SOLVER_ABSENT):
            problems.append(f"M8 solver {tool['name']} answered {status}, which is not a decision")
    if _quiet(lambda: command_solver_status(manifest, "no-such-solver")) != SOLVER_REFUSED:
        problems.append("M8 an undeclared solver was admitted instead of refused")

    problems.extend(_third_outcome_problems())
    problems.extend(_host_resource_problems())

    if problems:
        print("gate_tools self-test:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        return 1
    print(
        f"gate_tools self-test: {len(manifest['tools'])} declared tools each locate by the "
        f"method they state or are absent with a named skip"
    )
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, add_help=True, description=__doc__)
    parser.add_argument("action", nargs="?", default="report")
    parser.add_argument("name", nargs="?")
    parser.add_argument("--gate")
    parser.add_argument("--stage")
    parser.add_argument("--path")
    parser.add_argument("--minimum-gigabytes", type=float)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--profile", choices=('cargo', 'qualification'), default='cargo')
    parser.add_argument("--require-host", action="store_true")
    parser.add_argument("--observe", action="store_true")
    arguments = parser.parse_args(argv)

    manifest = load()
    if arguments.self_test:
        return self_test()
    action = arguments.action
    if action == "report":
        return command_report(manifest)
    if action == "gates":
        print("\n".join(gate_names(manifest)))
        return 0
    if action == "gates-of":
        tool = find_tool(manifest, arguments.name or "")
        if tool is None:
            return 1
        print(" ".join(tool["gates"]))
        return 0
    if action == "rows":
        for tool in manifest["tools"]:
            print(f"{tool['name']}|{' '.join(tool['gates'])}|{tool['install']}")
        return 0
    if action == "locate":
        return command_locate(manifest, arguments.name or "")
    if action == "require":
        return command_require(manifest, arguments.gate or arguments.name or "")
    if action == "solver-status":
        return command_solver_status(manifest, arguments.name or "")
    if action == "disk":
        return command_disk(
            manifest, arguments.stage or arguments.name or "", arguments.path,
            arguments.minimum_gigabytes,
        )
    if action == 'resources':
        return command_resources(manifest, arguments.profile, require_host=arguments.require_host,
                                 observe=arguments.observe)
    parser.print_usage(sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
