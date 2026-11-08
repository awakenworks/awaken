#!/usr/bin/env python3
"""The suite table of the service-backed groups (postgres, reliability) and the affected plan over it.

scripts/ci/service_suites.toml lists every suite of pg_tests.sh and
check-reliability.sh, in the order the whole group runs them: a stable id, the
phase (shell step) that runs it, and either the arguments its runner receives
(its packages are their -p/--package/--test-package values) or, for a step the
group script runs itself (object store, recovery drill, fuzz, sanitizers,
mutants), its packages. The shells read the rows with --rows and run them by
id; --group/--base says which ids a change since <base> can reach.

Row schema:
  R1 id is unique and starts with its group; phase is a shell step name
  R2 args are literal words that name their packages; a step row has no args
     and names its packages, or says in whole_only why it has none
  R3 env holds literal NAME=value pairs; every package is a workspace member,
     and a fuzz row names exactly the fuzz crate's path dependencies
Scope decision table (one change set, one group):
  S1 no path the group reads changed                              -> none
  S2 the runner: the entry, anything it sources, runs or imports
     (this planner too), its caller, or data a group script names  -> whole
  S3 a manifest, the lockfile, .config, the toolchain or a Rust
     input no workspace crate owns                                 -> whole
  S4 member-crate sources -> rows whose packages are owners or consumers
  S5 the table: rows of this group added or edited -> those rows; a reorder
     or a base table that is absent or does not load -> whole; a comment or
     another group's row -> nothing
"""
from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[2]
TABLE = "scripts/ci/service_suites.toml"
INPUT_GLOBS = ("crates/**/Cargo.toml", "fuzz/Cargo.toml", "scripts/ci/service_suites.toml", "scripts/ci/*.py", "scripts/ci/*.sh",
               "scripts/ci/pre-commit/*.sh", "scripts/ci/pre-push/*.sh")
GROUPS = {"postgres": ("pg_tests.sh", ("pg_required_test", "pg_test")),
          "reliability": ("check-reliability.sh", ("run_required_rust_tests",))}
CALLERS = ("check-all.sh",)
OWNED = "changed owners and transitive consumers, including test dependencies"
RULE = ("rule: every suite of a service group is a row of scripts/ci/service_suites.toml that names its packages\n"
        "why: an affected run skips what a change cannot reach, so a suite without packages would hide a red suite")
UNSAFE = re.compile(r"[$`;|&<>*?~\\\n\[\]{}()'\"]")
KEYS = {"id", "phase", "runner", "env", "args", "packages", "whole_only"}


class PlanError(ValueError):
    pass


@dataclass(frozen=True)
class Row:
    group: str
    id: str
    phase: str
    runner: str  # "step": the group script runs its own command under this id
    env: tuple[tuple[str, str], ...]
    args: tuple[str, ...]
    packages: tuple[str, ...]
    whole_only: str = ""

    def line(self) -> str:
        """``id phase env runner args``, shell-quoted: what the group scripts read."""
        return shlex.join([self.id, self.phase, " ".join(f"{k}={v}" for k, v in self.env), self.runner, *self.args])

    def summary(self) -> str:
        return " ".join(self.args)[:100] if self.args else f"step of {GROUPS[self.group][0]}"


def _named_packages(words: tuple[str, ...]) -> tuple[str, ...]:
    found = [words[i + 1] for i, word in enumerate(words[:-1]) if word in ("-p", "--package", "--test-package")]
    found += [word.split("=", 1)[1] for word in words if word.startswith(("--package=", "--test-package="))]
    return tuple(sorted(set(found)))


def load(text: str) -> list[Row]:
    """Rows of a table text, schema-checked (R1, R2, R3 env); a violation is a PlanError."""
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise PlanError(f"{TABLE}: {error}") from error
    errors, rows, seen = [f"{TABLE}: unknown group {key!r}" for key in data if key not in GROUPS], [], set()
    for group, (_, runners) in GROUPS.items():
        for index, item in enumerate(data.get(group, [])):
            ident, phase, env = str(item.get("id", "")), str(item.get("phase", "")), item.get("env", {})
            problems = [f"unknown keys {sorted(set(item) - KEYS)}"] if set(item) - KEYS else []
            if not re.fullmatch(rf"{group}\.[a-z0-9][a-z0-9.-]*", ident) or ident in seen:
                problems.append(f"id must be unique and start with '{group}.'")
            if not re.fullmatch(r"[a-z][a-z_]*", phase):
                problems.append("phase must name the shell step that runs the row")
            if not isinstance(env, dict) or not all(re.fullmatch(r"[A-Z][A-Z0-9_]*", str(k)) and
                                                    re.fullmatch(r"[A-Za-z0-9_.,:/=+-]*", str(v)) for k, v in env.items()):
                problems.append("env must map NAME to a literal value")
                env = {}
            words: tuple[str, ...] = ()
            if "args" in item:
                words = tuple(shlex.split(str(item["args"])))
                runner, packages = item.get("runner", runners[0]), _named_packages(words)
                if runner not in runners:
                    problems.append(f"runner must be one of {', '.join(runners)}")
                if any(UNSAFE.search(word) for word in words):
                    problems.append("args must be literal words: no expansion, quoting or shell syntax")
                if not packages:
                    problems.append("args name no package with -p/--package/--test-package")
                if {"packages", "whole_only"} & set(item):
                    problems.append("a row with args takes its packages from them")
            else:
                runner, packages = "step", tuple(item.get("packages", ()))
                if ("runner" in item or not isinstance(item.get("packages"), list)
                        or not all(isinstance(package, str) for package in packages)
                        or (not packages) != bool(item.get("whole_only"))):
                    problems.append("a step row names its packages, or says in whole_only why it has none")
            seen.add(ident)
            errors += [f"{TABLE}: [[{group}]] #{index + 1} {ident or '?'}: {problem}" for problem in problems]
            rows.append(Row(group, ident, phase, runner, tuple(sorted(env.items())), words,
                            tuple(packages), str(item.get("whole_only", ""))))
    if errors:
        raise PlanError("\n".join(errors))
    return rows


def group_rows(group: str, root: Path = ROOT) -> list[Row]:
    return [row for row in load((root / TABLE).read_text(encoding="utf-8")) if row.group == group]


def validate(rows: list[Row], root: Path = ROOT) -> None:
    """R3: packages are workspace members; fuzz rows name the fuzz crate's path dependencies."""
    from _crate_boundary_workspace import crate_manifests
    def name(manifest: Path) -> str:
        return tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]["name"]
    known = {name(path) for path in crate_manifests(root / "crates")}
    fuzz = tomllib.loads((root / "fuzz/Cargo.toml").read_text(encoding="utf-8")).get("dependencies", {})
    paths = sorted(name(root / "fuzz" / value["path"] / "Cargo.toml")
                   for value in fuzz.values() if isinstance(value, dict) and "path" in value)
    errors = [f"{TABLE}: {row.id}: unknown package {package}" for row in rows for package in row.packages
              if package not in known]
    errors += [f"{TABLE}: {row.id}: a fuzz row names the fuzz crate's path dependencies {paths}"
               for row in rows if ".fuzz." in row.id and sorted(row.packages) != paths]
    if errors:
        raise PlanError("\n".join(errors))


def changed_rows(group: str, base_text: str | None, text: str) -> tuple[set[str] | None, str]:
    """S5: ids of this group's rows the table change adds or edits, or (None, why whole)."""
    if base_text is None:
        return None, "absent at the base"
    try:
        old = {row.id: row for row in load(base_text) if row.group == group}
    except PlanError:
        return None, "does not load at the base"
    new = {row.id: row for row in load(text) if row.group == group}
    if [ident for ident in old if ident in new] != [ident for ident in new if ident in old]:
        return None, f"reorders {group} rows"
    return {ident for ident, row in new.items() if old.get(ident) != row}, ""


def group_files(group: str, scripts: Path) -> set[str]:
    """S2: the entry, its whole script closure and its callers, as repository paths."""
    import generate_hooks
    entry = GROUPS[group][0]
    return {f"scripts/ci/{name}" for name in {entry, *generate_hooks.script_closure(entry, scripts), *CALLERS}} - {TABLE}


def names_data(texts: list[str], path: str) -> bool:
    """A full repository path, or a scripts/ci file's own name as a whole word."""
    name = path.removeprefix("scripts/ci/")
    return any(path in text or ("/" not in name and re.search(rf"(?<![\w.-]){re.escape(name)}(?![\w.-])", text))
               for text in texts)


def classify(group: str, changed: list[str], metadata_loader, base_table, root: Path = ROOT):
    """``(whole reason or "", affected packages, edited row ids, reason, explain lines)``."""
    import generate_hooks
    import rust_test_plan
    own = group_files(group, root / "scripts/ci")
    texts = [(root / path).read_text(encoding="utf-8", errors="replace") for path in sorted(own) if (root / path).is_file()]
    whole, built, edited, lines = "", [], set(), []
    for path in changed:
        if path == TABLE:
            found, why = changed_rows(group, base_table(), (root / TABLE).read_text(encoding="utf-8"))
            why = f"suite table {why} -> whole group" if found is None else f"suite table -> its {len(found)} edited {group} row(s)"
            edited |= found or set()
        elif path in own:
            why = "group script -> whole group"
        elif path.startswith("scripts/") and not path.endswith((".sh", ".py")) and names_data(texts, path):
            why = "data a group script names -> whole group"
        elif any(generate_hooks.glob_matches(glob, path) for glob in rust_test_plan.rust_inputs()):
            why = "Rust build input -> owners and consumers (whole if unowned or a manifest)"
            built.append(path)
        else:
            why = "not read by this group -> ignored"
        if "whole group" in why and not whole:
            whole = f"{why.split(' ->')[0]}: {path}"
        lines.append(f"explain: {path}: {why}")
    if whole:
        return whole, [], edited, whole, lines
    if not built:
        return "", [], edited, "suite table rows edited" if edited else "no input of this group changed", lines
    selected, reason = rust_test_plan.affected_packages(metadata_loader(), built, root)
    return ("" if reason == OWNED else reason), selected, edited, reason, lines


def decide(rows: list[Row], whole: str, affected: list[str], edited: set[str]) -> list[tuple[Row, bool, str]]:
    decisions = []
    for row in rows:
        hit = sorted(set(row.packages) & set(affected))
        if whole:
            decisions.append((row, True, whole))
        elif row.id in edited:
            decisions.append((row, True, "its row was edited in the suite table"))
        elif hit:
            decisions.append((row, True, f"reached: {', '.join(hit)}"))
        elif not row.packages:
            decisions.append((row, False, f"runs with the whole group only: {row.whole_only}"))
        else:
            decisions.append((row, False, f"not reached: {', '.join(row.packages)}"))
    return decisions


def render(whole: str, reason: str, decisions) -> list[str]:
    selected = sum(run for _, run, _ in decisions)
    scope = "whole" if whole else "packages" if selected else "none"
    lines = [f"scope={scope}", f"reason={reason}", f"selected={selected}/{len(decisions)}"]
    return lines + [f"{'run ' if run else 'skip'} {row.id} {why} | {row.summary()}" for row, run, why in decisions]


def workspace_metadata() -> dict:
    import crate_test
    environment = crate_test.environment(ROOT)
    # Every feature and platform, as the Rust gate scope does.
    return json.loads(subprocess.check_output(crate_test.native_command(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--all-features"], environment),
        cwd=ROOT, env=environment))


# Trace: run a group script with stand-in tools. Each appends "<suite id>|<watched env>|<argv>"
# to $TRACE; the id is the AWAKEN_SERVICE_SUITE the shell exports while a suite runs.
WATCHED = ("AWAKEN_CRASH_BACKEND", "AWAKEN_SESSION_PERF_ROOTS", "RUSTFLAGS")
LOG = ('log() { local e="" n; for n in ' + " ".join(WATCHED) + '; do [ -z "${!n+x}" ] || e+="$n=${!n} "; done\n'
       '  local a="$*"; printf "%s|%s|%s\\n" "${AWAKEN_SERVICE_SUITE:-}" "$e" "${a//$\'\\n\'/ }" >> "$TRACE"; }\n'
       # Deadline wrappers put their own options before `--` (`SECONDS --`, `--admit SECONDS --`).
       'bounded() { while [ $# -gt 0 ] && [ "$1" != -- ]; do shift; done; shift\n'
       '  case "$1" in docker) exec "$@" ;; esac; log "$@"; exit 0; }\n')
STUBS = {
    "cargo": '[ "$1" != awaken-ci-deadline ] || bounded "${@:2}"\nlog cargo "$@"\n'
             'case "$*" in "install --list") echo "cargo-fuzz 1"; echo "cargo-mutants 1" ;; esac\n',
    "python3": 'case "$1" in -c|-|*service_test_plan.py) exec "$REAL_PYTHON" "$@" ;; *deadline.py) bounded "${@:2}" ;; esac\n'
               'log python3 "$@"\n',
    "docker": 'log docker "$@"\ncase "$1" in port) echo 127.0.0.1:55432 ;; esac\n',
    "rustup": 'log rustup "$@"\necho nightly-x86_64-unknown-linux-gnu\n',
    "rustc": 'echo "host: x86_64-unknown-linux-gnu"\n',
}
VOLATILE = ((re.compile(r"\S*/rust-execution\.\w{6}"), "<evidence>"), (re.compile(r"\S*/awaken-pg-recovery\.\w{6}"), "<recovery>"),
            (re.compile(r"awaken-ci-(pg|object)-\d+-[0-9a-f]{12}"), r"awaken-ci-\1-<run>"),
            (re.compile(r"\b[0-9a-f]{32}\b"), "<owner>"), (re.compile(r"rotation-\d+"), "rotation-<pid>"))


def _outside_inputs(directory: str, names: list[str]) -> set[str]:
    """copytree filter: keep the scripts/ci files INPUT_GLOBS covers (and the hook subdirectories)."""
    here = Path(directory)
    keep = {"pre-commit", "pre-push"} if here.name == "ci" else set()
    return {name for name in names if name not in keep
            and not (here.name in ("ci", "pre-commit", "pre-push") and (name.endswith((".py", ".sh"))
                     or (here.name == "ci" and name == "service_suites.toml")))}


def trace(source: Path, group: str, arguments: list[str]) -> tuple[int, list[tuple[str, str, str]]]:
    """``(status, [(suite id, env, command)])`` of one group run over a copy of ``source``'s scripts."""
    with tempfile.TemporaryDirectory(prefix="service-trace-") as directory:
        base = Path(directory)
        # Copy exactly what INPUT_GLOBS declares, so the trace reads nothing its trigger misses.
        shutil.copytree(source / "scripts/ci", base / "repo/scripts/ci", ignore=_outside_inputs)
        for path in ("repo/fuzz", "bin", "tmp"):
            (base / path).mkdir(parents=True)
        for name, body in STUBS.items():
            (base / "bin" / name).write_text("#!/usr/bin/env bash\n" + LOG + body, encoding="utf-8")
            (base / "bin" / name).chmod(0o755)
        environment = {"PATH": f"{base / 'bin'}:/usr/bin:/bin", "HOME": directory, "TRACE": f"{directory}/trace",
                       "REAL_PYTHON": sys.executable, "TMPDIR": f"{directory}/tmp", "CARGO_TARGET_DIR": f"{directory}/target"}
        if group == "postgres" and arguments != ["--require-docker"]:
            environment["AWAKEN_TEST_DATABASE_URL"] = "postgres://fixture"
        result = subprocess.run(["bash", "scripts/ci/" + GROUPS[group][0], *arguments], cwd=base / "repo",
                                env=environment, capture_output=True, text=True, timeout=600)
        text = (base / "trace").read_text(encoding="utf-8") if (base / "trace").exists() else ""
        text = text.replace(directory, "<tmp>")
        for pattern, replacement in VOLATILE:
            text = pattern.sub(replacement, text)
        if result.returncode:
            sys.stderr.write(result.stdout[-1500:] + result.stderr[-1500:])
        return result.returncode, [tuple(line.split("|", 2)) for line in text.splitlines()]


def runner_commands(row: Row) -> list[str]:
    """What a row's runner hands the stand-in tools."""
    if row.runner == "pg_test":
        return [" ".join(row.args)]
    return [" ".join(["python3 scripts/ci/nextest_execution.py build --", *row.args]),
            " ".join(["python3 scripts/ci/nextest_execution.py run --evidence <evidence> --", *row.args])]


TRACED = {"postgres": (["--require-docker"], ["--storage-observations"]), "reliability": (["--require-tools"],)}
# E1-E3 are Unix-only, like the other POSIX self-test parts (`skipIf(os.name == "nt", ...)`): the
# trace runs the group's Bash script with Bash stand-in tools first on a POSIX PATH. The groups
# themselves run only in the Linux gate; every other row of this self-test runs on native Windows.
TRACE_UNIX_ONLY = ("E1-E3 run the group's Bash script with Bash stand-in tools on a POSIX PATH; "
                   "the postgres and reliability groups run only in the Linux gate")


def equivalence(root: Path = ROOT) -> list[str]:
    """E1 a row's runner receives exactly its args and env; E2 nothing runs under an unknown
    id; E3 the whole group (plus the focused observation run) runs every row once, in table order."""
    failures = []
    for group, runs in TRACED.items():
        rows, ran = {row.id: row for row in group_rows(group, root)}, []
        for arguments in runs:
            status, lines = trace(root, group, arguments)
            if status:
                failures.append(f"E3 {group} {' '.join(arguments)} exited {status} under stand-in tools")
            for ident, env, command in lines:
                if ident and ident not in rows:
                    failures.append(f"E2 {group}: {command!r} ran under unknown suite {ident}")
                elif ident:
                    if not ran or ran[-1][0] != ident:
                        ran.append((ident, []))
                    ran[-1][1].append((env, command))
        if [ident for ident, _ in ran] != list(rows):
            failures.append(f"E3 {group} ran {[ident for ident, _ in ran]}, not every row once in table order")
        for ident, commands in ran:
            row = rows.get(ident)
            if row and row.runner != "step":
                want = "".join(f"{k}={v} " for k, v in row.env)
                if [command for _, command in commands] != runner_commands(row) or any(env != want for env, _ in commands):
                    failures.append(f"E1 {ident}: ran {commands}, not {runner_commands(row)} with env {want!r}")
    return failures


def self_test() -> int:
    failures: list[str] = []

    def expect(condition: bool, label: str) -> None:
        if not condition:
            failures.append(label)

    def refuses(text: str, needle: str, label: str) -> None:
        try:
            validate(load(text), root)
            failures.append(f"{label}: accepted")
        except PlanError as error:
            expect(needle in str(error), f"{label}: {error}")

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        scripts = root / "scripts/ci"
        scripts.mkdir(parents=True)
        names = {"kernel": (), "store-a": ("kernel",), "store-b": (), "app": ("store-a",)}
        for name in names:
            (root / "crates" / name).mkdir(parents=True)
            (root / "crates" / name / "Cargo.toml").write_text(f'[package]\nname = "{name}"\n', encoding="utf-8")
        (root / "fuzz").mkdir()
        (root / "fuzz/Cargo.toml").write_text('[dependencies]\nk = { path = "../crates/kernel" }\n', encoding="utf-8")
        metadata = {"workspace_members": list(names),
                    "packages": [{"id": n, "name": n, "manifest_path": str(root / "crates" / n / "Cargo.toml")} for n in names],
                    "resolve": {"nodes": [{"id": n, "deps": [{"pkg": d} for d in deps]} for n, deps in names.items()]}}
        (scripts / "pg_tests.sh").write_text('source scripts/ci/_affected_suites.sh\ndocker run -v "$PWD/scripts/ci/policy.json:/p" i\n',
                                             encoding="utf-8")
        (scripts / "_affected_suites.sh").write_text("python3 scripts/ci/service_test_plan.py --rows x\n", encoding="utf-8")
        (scripts / "check-all.sh").write_text("run postgres postgres scripts/ci/pg_tests.sh\n", encoding="utf-8")
        for name in ("policy.json", "unrelated.json", "unrelated.py"):
            (scripts / name).write_text("{}\n", encoding="utf-8")
        table = ("[[postgres]]\nid = \"postgres.a\"\nphase = \"p\"\nargs = '''\n-p store-a --features postgres\n"
                 "--require-case=a -- --test-threads=1\n'''\n\n"
                 "[[postgres]]\nid = \"postgres.b\"\nphase = \"p\"\nrunner = \"pg_test\"\nargs = 'cargo test -p store-b'\n\n"
                 "[[postgres]]\nid = \"postgres.app\"\nphase = \"p\"\nenv = { AWAKEN_CRASH_BACKEND = \"postgres\" }\n"
                 "args = '-p app --locked'\n\n"
                 "[[postgres]]\nid = \"postgres.object\"\nphase = \"object\"\npackages = [\"store-b\"]\n\n"
                 "[[postgres]]\nid = \"postgres.drill\"\nphase = \"drill\"\npackages = []\nwhole_only = \"restores the substrates\"\n\n"
                 "[[reliability]]\nid = \"reliability.fuzz.k\"\nphase = \"tools\"\npackages = [\"kernel\"]\n\n"
                 "[[reliability]]\nid = \"reliability.k\"\nphase = \"quick\"\nargs = '-p kernel --lib'\n")
        (scripts / "service_suites.toml").write_text(table, encoding="utf-8")
        loader = lambda: metadata  # noqa: E731

        def plan(changed: list[str], base: str | None = table, group: str = "postgres"):
            whole, affected, edited, reason, lines = classify(group, changed, loader, lambda: base, root)
            decisions = decide(group_rows(group, root), whole, affected, edited)
            return whole, {row.id.split(".", 1)[1]: run for row, run, _ in decisions}, render(whole, reason, decisions) + lines

        rows = group_rows("postgres", root)
        validate(load(table), root)
        expect([(r.id, r.runner, r.packages, r.env) for r in rows] == [
            ("postgres.a", "pg_required_test", ("store-a",), ()), ("postgres.b", "pg_test", ("store-b",), ()),
            ("postgres.app", "pg_required_test", ("app",), (("AWAKEN_CRASH_BACKEND", "postgres"),)),
            ("postgres.object", "step", ("store-b",), ()), ("postgres.drill", "step", (), ())], f"schema rows: {rows}")
        expect(rows[2].line() == "postgres.app p AWAKEN_CRASH_BACKEND=postgres pg_required_test -p app --locked"
               and rows[3].line() == "postgres.object object '' step", f"--rows lines: {rows[2].line()} / {rows[3].line()}")
        # T1 a doc-only (or unrelated-script) change selects nothing.
        whole, runs, _ = plan(["docs/design/a.md", "web/src/x.ts", "scripts/ci/unrelated.py", "scripts/ci/unrelated.json"])
        expect(not whole and not any(runs.values()), f"T1 doc-only selects nothing: {runs}")
        # T2 one crate selects its rows and its consumers' rows only.
        whole, runs, _ = plan(["crates/store-b/src/lib.rs"])
        expect(runs == {"a": False, "b": True, "app": False, "object": True, "drill": False}, f"T2 store-b: {runs}")
        whole, runs, _ = plan(["crates/kernel/src/lib.rs", "docs/x.md"])
        expect(runs == {"a": True, "b": False, "app": True, "object": False, "drill": False}, f"T2 consumers: {runs}")
        # T3 lockfile, manifest, profile, the runner, its helper, caller and named data select everything.
        for path in ("Cargo.lock", "crates/store-b/Cargo.toml", ".config/nextest.toml", "scripts/ci/pg_tests.sh",
                     "scripts/ci/_affected_suites.sh", "scripts/ci/check-all.sh", "scripts/ci/policy.json",
                     "fuzz/fuzz_targets/x.rs"):
            whole, runs, _ = plan([path, "crates/store-b/src/lib.rs"])
            expect(bool(whole) and all(runs.values()), f"T3 {path} selects everything: {runs}")
        # D1 a row edit runs that row only; D2 a comment or another group's row runs nothing;
        # D3 an added row runs; D4 a reorder, an absent or unreadable base table runs everything.
        edited = table.replace("--require-case=a ", "--require-case=a --require-case=a2 ")
        (scripts / "service_suites.toml").write_text(edited, encoding="utf-8")
        whole, runs, lines = plan([TABLE])
        expect(not whole and runs == {"a": True, "b": False, "app": False, "object": False, "drill": False},
               f"D1 a row edit runs that row: {runs}")
        expect("selected=1/5" in lines and any("run  postgres.a its row was edited" in line for line in lines),
               f"D1 explain: {lines[:4]}")
        whole, runs, _ = plan([TABLE, "crates/store-b/src/lib.rs"])
        expect(runs == {"a": True, "b": True, "app": False, "object": True, "drill": False}, f"D1 with a crate: {runs}")
        for label, text in (("D2 comment", "# a note\n" + table.replace("phase = \"drill\"", "phase = \"drill\" # why")),
                            ("D2 other group", table.replace("-p kernel --lib", "-p kernel --lib --bins")),
                            ("D2 relaid words", table.replace("--features postgres\n", "--features postgres "))):
            (scripts / "service_suites.toml").write_text(text, encoding="utf-8")
            whole, runs, _ = plan([TABLE])
            expect(not whole and not any(runs.values()), f"{label} runs nothing: {runs}")
        (scripts / "service_suites.toml").write_text(table + "\n[[postgres]]\nid = \"postgres.c\"\nphase = \"p\"\n"
                                                     "args = '-p store-b --lib'\n", encoding="utf-8")
        whole, runs, _ = plan([TABLE])
        expect(not whole and [k for k, v in runs.items() if v] == ["c"], f"D3 an added row runs: {runs}")
        for label, text, base in (("D4 reorder", table, table.replace("postgres.a\"", "postgres.z\"").replace(
                                       "postgres.b\"", "postgres.a\"").replace("postgres.z\"", "postgres.b\"")),
                                  ("D4 absent base", table, None), ("D4 unreadable base", table, "[[postgres]]\nid = 3\n")):
            (scripts / "service_suites.toml").write_text(text, encoding="utf-8")
            whole, runs, _ = plan([TABLE], base)
            expect(bool(whole) and all(runs.values()), f"{label} runs everything: {runs}")
        (scripts / "service_suites.toml").write_text(table, encoding="utf-8")
        # T5 explain names each row, its verdict and why, and each path's class.
        _, _, lines = plan(["crates/store-b/src/lib.rs", "docs/a.md"])
        for needle in ("scope=packages", "selected=2/5", "run  postgres.b reached: store-b", "skip postgres.a not reached: store-a",
                       "skip postgres.drill runs with the whole group only", "explain: docs/a.md: not read by this group -> ignored"):
            expect(needle in "\n".join(lines), f"T5 explain lacks {needle!r}")
        # U1-U8 a row the planner cannot map is refused, never skipped.
        row = "[[postgres]]\nid = \"postgres.x\"\nphase = \"p\"\n"
        for text, needle, label in (
                (row + "args = '--lib only'\n", "name no package", "U1 args without a package"),
                (row + "args = '-p ghost --lib'\n", "unknown package ghost", "U2 unknown package"),
                (row + "args = '-p app $EXTRA'\n", "literal words", "U3 shell expansion"),
                (row + "packages = []\n", "whole_only", "U4 step without packages or reason"),
                (row + "args = '-p app'\n" + row + "args = '-p app'\n", "unique", "U5 duplicate id"),
                (row + "args = '-p app'\nrunner = \"bash\"\n", "runner must be", "U6 unknown runner"),
                ("[[kubernetes]]\nid = \"kubernetes.x\"\n", "unknown group", "U7 unknown group"),
                ("[[reliability]]\nid = \"reliability.fuzz.x\"\nphase = \"t\"\npackages = [\"app\"]\n", "fuzz crate", "U8 fuzz")):
            refuses(text, needle, label)
        failures += _helper_test(root)
    # The real table: every row maps to workspace packages, and the shells run exactly the table.
    try:
        real = load((ROOT / TABLE).read_text(encoding="utf-8"))
        validate(real, ROOT)
        counts = {group: sum(row.group == group for row in real) for group in GROUPS}
        expect(counts["postgres"] >= 40 and counts["reliability"] >= 20, f"real row counts {counts}")
        if os.name == "nt":
            print(f"service_test_plan self-test: skip E1-E3 on native Windows: {TRACE_UNIX_ONLY}")
        else:
            failures += equivalence(ROOT)
    except PlanError as error:
        failures.append(f"real table: {error}")
    caller = (ROOT / "scripts/ci/check-all.sh").read_text(encoding="utf-8")
    expect('[ "$full_gate" -eq 0 ]' in caller.split("AWAKEN_SERVICE_AFFECTED_BASE", 1)[-1][:200],
           "check-all.sh must never narrow --full-gate")
    for failure in failures:
        print(f"service_test_plan self-test: FAIL {failure}", file=sys.stderr)
    if not failures:
        traced = "" if os.name == "nt" else ", E1-E3"
        print(f"service_test_plan self-test: R1-R3, S1-S5, T1-T5, D1-D4, U1-U8{traced} and the shell helper hold")
    return 1 if failures else 0


def _helper_test(root: Path) -> list[str]:
    """H1-H4: the real _affected_suites.sh runs rows by id and refuses an id it cannot place."""
    scripts = root / "scripts/ci"
    shutil.copy(ROOT / "scripts/ci/_affected_suites.sh", scripts / "_affected_suites.sh")
    (scripts / "service_test_plan.py").write_text(
        "import sys\nif sys.argv[1] == '--rows':\n"
        "    print(\"x.run p '' probe --flag 'two words'\\nx.skip q A=1 probe y\\nx.step s '' step\")\n"
        "else:\n    print('scope=packages\\nselected=2/3\\nrun  x.run r | a\\nskip x.skip s | b\\nrun  x.step r | c')\n",
        encoding="utf-8")
    (scripts / "probe.sh").write_text(
        "set -euo pipefail\nsource scripts/ci/_affected_suites.sh\nsuites_load x\n"
        'probe() { printf "%s|%s|%s\\n" "$AWAKEN_SERVICE_SUITE" "${A:-}" "$*" >> calls; }\n'
        '[ "${NARROW:-0}" = 0 ] || affected_suites_load x HEAD >/dev/null\n'
        "suite_run x.run\nsuite_run x.skip\nsuite_step x.step probe stepped\n"
        "suites_selected p && echo p >> calls\nsuites_selected q && echo q >> calls\n"
        'suite_step "${UNKNOWN:-x.step}" probe last\n', encoding="utf-8", newline="\n")
    failures = []

    def probe(**environment: str) -> tuple[subprocess.CompletedProcess, str]:
        (root / "calls").unlink(missing_ok=True)
        result = subprocess.run([sys.executable, str(ROOT / "scripts/ci/_native_shell.py"), "scripts/ci/probe.sh"],
                                cwd=root, capture_output=True, text=True,
                                env={"PATH": f"{Path(sys.executable).parent}:/usr/bin:/bin", **environment})
        return result, (root / "calls").read_text() if (root / "calls").exists() else ""

    result, calls = probe()
    if result.returncode or calls != "x.run||--flag two words\nx.skip|1|y\nx.step||stepped\np\nq\nx.step||last\n":
        failures.append(f"H1 rows run by id with their env: exit {result.returncode} {calls!r} {result.stderr}")
    result, calls = probe(NARROW="1")
    if result.returncode or calls != "x.run||--flag two words\nx.step||stepped\np\nx.step||last\n":
        failures.append(f"H2 an affected run skips only unselected ids: exit {result.returncode} {calls!r} {result.stderr}")
    result, calls = probe(UNKNOWN="x.ghost")
    if result.returncode != 2 or "x.ghost" not in result.stderr or "last" in calls:
        failures.append(f"H3 an id outside the table stops the run with 2: exit {result.returncode} {result.stderr}")
    result, _ = probe(NARROW="1", AWAKEN_BUILD_WORKLOAD_KIND="qualification")
    if result.returncode != 2 or "never narrowed" not in result.stderr:
        failures.append(f"H4 a qualification run is never narrowed: exit {result.returncode}")
    return failures


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__.splitlines()[0])
    parser.add_argument("--rows", choices=sorted(GROUPS), help="print the group's rows for its shell (id phase env runner args)")
    parser.add_argument("--group", choices=sorted(GROUPS))
    parser.add_argument("--base", help="revision the change is measured from (index, worktree and untracked included)")
    parser.add_argument("--explain", action="store_true", help="also print why each changed path counts (stderr)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test or args.rows:
        if args.group or args.base or (args.self_test and args.rows):
            parser.error("--self-test and --rows are standalone modes")
        if args.self_test:
            return self_test()
    elif not args.group or not args.base:
        parser.error("--group and --base are required")
    try:
        if args.rows:
            print("\n".join(row.line() for row in group_rows(args.rows)))
            return 0
        import rust_test_plan
        rows = group_rows(args.group)
        validate(rows)
        base = rust_test_plan.git_output("rev-parse", "--verify", "--end-of-options", args.base + "^{commit}").decode().strip()

        def base_table() -> str | None:
            shown = subprocess.run(["git", "show", f"{base}:{TABLE}"], cwd=ROOT, capture_output=True, text=True)
            return shown.stdout if shown.returncode == 0 else None

        whole, affected, edited, reason, lines = classify(args.group, rust_test_plan.changed_sources(base),
                                                          workspace_metadata, base_table)
    except PlanError as error:
        print(f"{error}\n{RULE}\nfix: name the package with -p in the row's args, or give a step row its packages",
              file=sys.stderr)
        return 2
    except subprocess.CalledProcessError as error:
        print(f"Cannot plan the affected {args.group} group (status {error.returncode}): {error.cmd}", file=sys.stderr)
        return error.returncode if error.returncode > 0 else 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"Cannot plan the {args.group or args.rows} group: {error}", file=sys.stderr)
        return 1
    if args.explain:
        print("\n".join(lines), file=sys.stderr)
    print("\n".join(render(whole, reason, decide(rows, whole, affected, edited))))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
