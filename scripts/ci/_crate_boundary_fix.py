"""Next steps and mechanical repairs for the refusals of ``check_crate_boundaries``.

Every refusal the boundary gate prints should end with one concrete next step.
Most of them already do (``...; fix: ...``); this module adds the step to the
ones that do not, naming the file that holds the list or the manifest that
declares the edge, and recognizes the refusals whose correct content follows
from the tree alone. ``check_crate_boundaries.py --fix`` applies only those:

| Refusal                                                  | Repair                                   |
| -------------------------------------------------------- | ---------------------------------------- |
| a ratchet fell: "--tighten to lower <baseline>"          | that guard's own ``--tighten``           |
| "<edge> no longer violates; remove it from <baseline>"   | delete that one baseline entry           |
| sdk-engine-spi "... below the baseline N"                | lower that one count to the measured one |
| ModelBinding::new "below the baseline of N"              | lower the constant to the measured count |
| dependency-use "only in test code"                       | move the entry to ``[dev-dependencies]`` |

Each repair only lowers or removes an allowance or narrows a normal
dependency edge; none adds an allowance, raises a count or records a new
crossing, so a real violation stays refused after ``--fix``. A dependency entry
that is optional, named by a ``[features]`` entry, target-specific or spread
over several lines is left for a person and the refusal says so.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

import _crate_boundary_workspace

COMMAND = "python3 scripts/ci/check_crate_boundaries.py --fix"
SCRIPTS = Path(__file__).resolve().parent
REPO_ROOT = SCRIPTS.parent.parent

TIGHTEN = re.compile(r"python3 scripts/ci/(?P<module>_\w+\.py) --tighten to lower")
STALE = re.compile(
    r"^(?P<rule>[\w-]+): (?P<key>\S+ -> \S+) no longer violates; remove it from (?P<baseline>\w+\.json)"
)
SPI_BELOW = re.compile(
    r"^sdk-engine-spi: (?P<crate>\S+) names (?P<part>\S+) at (?P<now>\d+) site\(s\), below the baseline \d+"
)
MODEL_BINDING_BELOW = re.compile(r"^(?P<now>\d+) ModelBinding::new call sites, below the baseline of \d+")
TEST_ONLY = re.compile(
    r"^dependency-use: (?P<crate>\S+) names (?P<dependency>\S+) only in test code; move it to \[dev-dependencies\]"
)
NEVER_NAMED = re.compile(r"^dependency-use: (?P<crate>\S+) never names (?P<dependency>\S+);")
NEW_EDGE = re.compile(r"^(?P<rule>[\w-]+): new dependency (?P<source>\S+) -> (?P<target>\S+);")


@dataclass(frozen=True)
class Repair:
    """One mechanical repair a refusal names; ``key`` identifies what it changes."""

    kind: str
    key: tuple[str, ...]


def repair_for(error: str) -> Repair | None:
    if match := TIGHTEN.search(error):
        return Repair("tighten", (match["module"],))
    if match := STALE.match(error):
        return Repair("stale-entry", (match["baseline"], match["rule"], match["key"]))
    if match := SPI_BELOW.match(error):
        return Repair("spi-count", (match["crate"], match["part"], match["now"]))
    if match := MODEL_BINDING_BELOW.match(error):
        return Repair("model-binding", (match["now"],))
    if match := TEST_ONLY.match(error):
        return Repair("dev-dependency", (match["crate"], match["dependency"]))
    return None


def _manifests(repo_root: Path) -> dict[str, Path]:
    found = {}
    for path in _crate_boundary_workspace.crate_manifests(repo_root / "crates"):
        found[_crate_boundary_workspace.package_name(_crate_boundary_workspace.load_manifest(path))] = path
    return found


# Refusals whose owner names the rule but not the file a person edits next.
# Each row is (pattern, next step); the step names the list or the decision.
GUIDANCE: tuple[tuple[re.Pattern[str], Callable[[re.Match[str]], str]], ...] = (
    (re.compile(r"^unlisted Awaken managed extension route: (?P<method>\S+) (?P<path>\S+)"),
     lambda m: f"if it is a deliberate Awaken extension, add (\"{m['method']}\", \"{m['path']}\") to "
               "AWAKEN_MANAGED_EXTENSION_ROUTES in scripts/ci/_managed_protocol_boundary.py; "
               "otherwise remove the route"),
    (re.compile(r"^missing listed Awaken managed extension route: (?P<method>\S+) (?P<path>\S+)"),
     lambda m: f"remove (\"{m['method']}\", \"{m['path']}\") from AWAKEN_MANAGED_EXTENSION_ROUTES in "
               "scripts/ci/_managed_protocol_boundary.py, or restore the route"),
    (re.compile(r"^unregistered public route owner source (?P<path>\S+); add it to ROUTE_OWNER_FILES"),
     lambda m: f"add \"{m['path']}\" to ROUTE_OWNER_FILES in scripts/ci/_execution_ownership_fitness.py"),
    (re.compile(r"isolates '(?P<dependency>[^']+)', which is not in ISOLATED_DEPENDENCIES"),
     lambda m: f"decide whether {m['dependency']} justifies a crate of its own: if so add it with that reason "
               "to ISOLATED_DEPENDENCIES in scripts/ci/_crate_separation_fitness.py; if not, merge the crate "
               "back into its context"),
    (re.compile(r"^(?P<path>\S+): production DDL is not owned by a versioned Migration"),
     lambda m: f"move the DDL in {m['path']} into a versioned migration (migrations/V####__name.sql "
               "registered through Migration::per_dialect); DDL a test builds belongs in #[cfg(test)] code "
               "or under tests/"),
)


def next_step(error: str, manifests: Callable[[], dict[str, Path]], repo_root: Path = REPO_ROOT) -> str | None:
    """The step to append, or None when the refusal already carries one."""
    repair = repair_for(error)
    if repair is not None and repair.kind == "dev-dependency":
        manifest = manifests().get(repair.key[0])
        where = f" in {manifest.relative_to(repo_root).as_posix()}" if manifest else ""
        return f"`{COMMAND}` moves it{where}"
    if repair is not None:
        return f"`{COMMAND}` records it"
    if match := NEVER_NAMED.match(error):
        manifest = manifests().get(match["crate"])
        if manifest:
            return f"edit [dependencies] in {manifest.relative_to(repo_root).as_posix()}"
    if match := NEW_EDGE.match(error):
        manifest = manifests().get(match["source"])
        if manifest:
            return f"the edge to {match['target']} is declared in {manifest.relative_to(repo_root).as_posix()}"
    for pattern, step in GUIDANCE:
        if match := pattern.search(error):
            return step(match)
    return None


def annotate(errors: list[str], repo_root: Path = REPO_ROOT) -> list[str]:
    """Each refusal with its next step appended where the owner gave none."""
    cache: dict[str, dict[str, Path]] = {}

    def manifests() -> dict[str, Path]:
        if "found" not in cache:
            cache["found"] = _manifests(repo_root)
        return cache["found"]

    annotated = []
    for error in errors:
        step = next_step(error, manifests, repo_root)
        annotated.append(f"{error}{' ' if error.endswith('.') else '; '}next: {step}" if step else error)
    return annotated


def summary(errors: list[str]) -> str:
    fixable = sum(repair_for(error) is not None for error in errors)
    text = f"crate-boundaries: {len(errors)} refusal(s)"
    if fixable:
        text += (f"; {fixable} of them are mechanical: run `{COMMAND}` (it only lowers baselines, drops entries "
                 "that no longer violate and moves test-only dependencies to [dev-dependencies]), then stage "
                 "what it changed")
    if fixable < len(errors):
        text += f"; {'the other ' if fixable else ''}{len(errors) - fixable} need the change named after `fix:`/`next:`"
    return text


# --- repairs ------------------------------------------------------------------------------------


class Refused(Exception):
    """A repair the tree does not determine; the message says what a person decides."""


def _write_json(path: Path, value: dict) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8", newline="\n")


def _tighten(module: str, repo_root: Path) -> str:
    completed = subprocess.run([sys.executable, str(SCRIPTS / module), "--tighten"], cwd=repo_root,
                               capture_output=True, text=True, check=False)
    if completed.returncode != 0:
        detail = (completed.stderr or completed.stdout).strip().splitlines()
        raise Refused(f"{module} --tighten refused: {detail[-1] if detail else completed.returncode}; "
                      "its other refusals must be fixed first")
    return f"{module} --tighten: {completed.stdout.strip().splitlines()[-1]}"


def _drop_stale(baseline: str, rule: str, key: str) -> str:
    path = SCRIPTS / baseline
    document = json.loads(path.read_text(encoding="utf-8"))
    section = "crate-dependencies" if baseline == "sdk_engine_spi_baseline.json" else rule
    if key not in document.get(section, {}):
        raise Refused(f"{baseline} has no {section} entry {key}")
    del document[section][key]
    _write_json(path, document)
    return f"removed {section} {key} from {baseline}"


def _lower_spi(crate: str, part: str, now: str) -> str:
    path = SCRIPTS / "sdk_engine_spi_baseline.json"
    document = json.loads(path.read_text(encoding="utf-8"))
    counts = document.get("spi-paths", {}).get(crate, {})
    held = counts.get(part)
    if held is None or int(now) >= held:
        raise Refused(f"sdk_engine_spi_baseline.json holds no larger count for {crate} {part}")
    if int(now):
        counts[part] = int(now)
    else:
        del counts[part]
        if not counts:
            del document["spi-paths"][crate]
    _write_json(path, document)
    return f"lowered sdk-engine-spi {crate} {part} {held} -> {now}"


MODEL_BINDING_CONSTANT = re.compile(r"^UNVALIDATED_MODEL_BINDING_BASELINE = (?P<held>\d+)$", re.M)


def _lower_model_binding(now: str, checker: Path) -> str:
    text = checker.read_text(encoding="utf-8")
    match = MODEL_BINDING_CONSTANT.search(text)
    if match is None or int(now) >= int(match["held"]):
        raise Refused("UNVALIDATED_MODEL_BINDING_BASELINE holds no larger count")
    checker.write_text(text[:match.start("held")] + now + text[match.end("held"):], encoding="utf-8", newline="\n")
    return f"lowered UNVALIDATED_MODEL_BINDING_BASELINE {match['held']} -> {now}"


SECTION = re.compile(r"^\s*\[\[?(?P<name>[^\[\]]+)\]\]?\s*(?:#.*)?$")


def move_to_dev_dependencies(text: str, dependency: str) -> str:
    """The manifest with ``dependency``'s one-line [dependencies] entry under [dev-dependencies]."""
    lines = text.splitlines(keepends=True)
    sections: list[tuple[str, int, int]] = []
    for number, line in enumerate(lines):
        if match := SECTION.match(line):
            if sections:
                sections[-1] = (sections[-1][0], sections[-1][1], number)
            sections.append((match["name"].strip(), number + 1, len(lines)))

    def entries(name: str) -> list[int]:
        found = []
        for section, start, end in sections:
            if section != name:
                continue
            for number in range(start, end):
                key = lines[number].split("=", 1)[0].strip()
                alias = key.split(".", 1)[0].strip('"')
                package = re.search(r'\bpackage\s*=\s*"([^"]+)"', lines[number])
                if "=" in lines[number] and not lines[number].lstrip().startswith("#") and (
                        (package and package[1] == dependency) or (not package and alias == dependency)):
                    found.append(number)
        return found

    normal = entries("dependencies")
    if len(normal) != 1:
        raise Refused(f"{dependency} is not one [dependencies] line (a target-specific or table entry); "
                      "move it by hand")
    line = lines[normal[0]]
    alias = line.split("=", 1)[0].strip().split(".", 1)[0].strip('"')
    if line.count("{") != line.count("}") or line.rstrip().endswith(("{", "[", ",")):
        raise Refused(f"{dependency} spans several lines; move it by hand")
    if re.search(r"\boptional\s*=\s*true\b", line):
        raise Refused(f"{dependency} is optional, which a dev-dependency cannot be; decide by hand")
    features = next((section for section in sections if section[0] == "features"), None)
    if features and any(re.search(rf'"(?:dep:)?{re.escape(alias)}[/"?]', lines[number])
                        for number in range(features[1], features[2])):
        raise Refused(f"{dependency} is named by [features]; decide by hand")
    del lines[normal[0]]
    sections = [(name, start - (start > normal[0]), end - (end > normal[0])) for name, start, end in sections]
    if entries("dev-dependencies"):
        return "".join(lines)
    dev = next((section for section in sections if section[0] == "dev-dependencies"), None)
    if dev is None:
        if lines and not lines[-1].endswith("\n"):
            lines[-1] += "\n"
        return "".join(lines) + f"\n[dev-dependencies]\n{line.rstrip()}\n"
    insert = dev[2]
    while insert > dev[1] and not lines[insert - 1].strip():
        insert -= 1
    lines.insert(insert, line if line.endswith("\n") else line + "\n")
    return "".join(lines)


def _dev_dependency(crate: str, dependency: str, repo_root: Path) -> str:
    manifest = _manifests(repo_root).get(crate)
    if manifest is None:
        raise Refused(f"no manifest declares package {crate}")
    text = manifest.read_text(encoding="utf-8")
    manifest.write_text(move_to_dev_dependencies(text, dependency), encoding="utf-8", newline="\n")
    return f"moved {dependency} to [dev-dependencies] in {manifest.relative_to(repo_root).as_posix()}"


def apply(errors: list[str], repo_root: Path = REPO_ROOT, checker: Path | None = None) -> tuple[list[str], list[str]]:
    """Apply each mechanical repair once; return (what changed, what a person must decide)."""
    done, refused = [], []
    for repair in dict.fromkeys(filter(None, map(repair_for, errors))):
        try:
            if repair.kind == "tighten":
                done.append(_tighten(repair.key[0], repo_root))
            elif repair.kind == "stale-entry":
                done.append(_drop_stale(*repair.key))
            elif repair.kind == "spi-count":
                done.append(_lower_spi(*repair.key))
            elif repair.kind == "model-binding":
                done.append(_lower_model_binding(repair.key[0], checker or SCRIPTS / "check_crate_boundaries.py"))
            elif repair.kind == "dev-dependency":
                done.append(_dev_dependency(*repair.key, repo_root))
        except (Refused, OSError, ValueError) as refusal:
            refused.append(f"{repair.kind} {' '.join(repair.key)}: {refusal}")
    return done, refused


def selftest() -> None:
    """Decision table: which refusal is mechanical, and what each repair writes.

    | Rule | refusal                                           | repair / effect                         |
    | X1   | a ratchet below its baseline (--tighten advice)   | that module's --tighten                 |
    | X2   | a baseline entry that no longer violates          | that entry removed, nothing else        |
    | X3   | a new crossing, a count above its baseline        | none: stays refused, gets a next step   |
    | X4   | test-only one-line dependency                     | moved under [dev-dependencies]          |
    | X5   | optional / features-named / multi-line dependency | refused, file unchanged                 |
    | X6   | dev entry already present                         | only the normal entry is removed        |
    """
    assert repair_for("naming: turn-noun: a::b 1 -> 0; fix: python3 scripts/ci/_naming_fitness.py --tighten to "
                      "lower naming_baseline.json in this commit") == Repair("tighten", ("_naming_fitness.py",)), "X1"
    assert repair_for("context-contract-seam: a -> b no longer violates; remove it from "
                      "bounded_context_baseline.json in this commit") == Repair(
        "stale-entry", ("bounded_context_baseline.json", "context-contract-seam", "a -> b")), "X2"
    for growth in ("context-contract-seam: new dependency a -> b; reach another bounded context only through ...",
                   "naming: turn-noun: a::turn 0 -> 1 (x.rs:1); fix: use Thread, Run or Step",
                   "405 ModelBinding::new call sites, baseline 404: 1 new producer(s) use panic-based admission.",
                   "sdk-engine-spi: a names b at 9 site(s), baseline 8; only Execution ..."):
        assert repair_for(growth) is None, f"X3 {growth}"
    manifest = ('[package]\nname = "a"\n\n[dependencies]\nb.workspace = true\nc = { workspace = true }\n\n'
                '[dev-dependencies]\nd.workspace = true\n')
    moved = move_to_dev_dependencies(manifest, "c")
    assert moved == ('[package]\nname = "a"\n\n[dependencies]\nb.workspace = true\n\n'
                     '[dev-dependencies]\nd.workspace = true\nc = { workspace = true }\n'), "X4 existing section"
    assert move_to_dev_dependencies('[dependencies]\nb.workspace = true\n', "b") == (
        "[dependencies]\n\n[dev-dependencies]\nb.workspace = true\n"), "X4 new section"
    assert move_to_dev_dependencies('[dependencies]\nx = { package = "b", path = "../b" }\n', "b").endswith(
        '[dev-dependencies]\nx = { package = "b", path = "../b" }\n'), "X4 renamed"
    for text in ('[dependencies]\nb = { workspace = true, optional = true }\n',
                 '[dependencies]\nb.workspace = true\n[features]\nf = ["dep:b"]\n',
                 '[dependencies]\nb = { workspace = true,\n  features = ["x"] }\n',
                 "[target.'cfg(unix)'.dependencies]\nb.workspace = true\n"):
        try:
            move_to_dev_dependencies(text, "b")
        except Refused:
            continue
        raise AssertionError(f"X5 {text!r}")
    assert move_to_dev_dependencies('[dependencies]\nb.workspace = true\n[dev-dependencies]\nb.workspace = true\n',
                                    "b") == "[dependencies]\n[dev-dependencies]\nb.workspace = true\n", "X6"
    print("_crate_boundary_fix selftest: the decision table holds")


if __name__ == "__main__":
    selftest()
