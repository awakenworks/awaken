#!/usr/bin/env python3
"""Render every lefthook trigger glob from the checker that owns the inputs.

A file-triggered hook is a gate whose *trigger set* must contain its *input
set*.  Hand-writing the trigger makes the two drift silently: the check simply
never runs on the change that needed it, and nothing reports that.  Two live
instances of exactly that shape reached ``main`` (a formal-surface hook that
globbed only ``formal/*.json`` while the checker reads every production Rust
file, and an oracle hook that did not glob ``e2e/package.json``).

So the glob is not authored by hand at all.  Each checker declares the paths it
reads as a module-level ``INPUT_GLOBS`` tuple next to its docstring, and this
script renders the ``glob:`` line of every ``pre-commit`` command that invokes
such a checker, between generated markers.  ``--check`` fails with a diff when
``lefthook.yml`` is stale, in the same shape as
``scripts/contract/generate-contracts.sh --check``.

Scope and its reasons:

* Only ``pre-commit`` commands are rendered.  ``commit-msg`` and ``pre-push``
  commands have no staged-file trigger set, so there is nothing to narrow: the
  commit-message validator takes the message file as ``{1}``, and the push gate
  runs unconditionally.
* Only the ``glob:`` line is generated. Command names, ``run:`` bodies and the
  surrounding comments stay human-authored. The Rust shell gate's literal mode
  arms and guards select its Python input owners; unsupported control shapes
  refuse generation. Other shell scripts and non-Python tools stay untouched.
* The invoked checker's own path is added to its rendered glob automatically
  (unless an already-declared pattern matches it), because editing a checker
  changes what it accepts.
* ``INPUT_GLOBS = ("**/*",)`` is the sentinel for "reads every staged file".
  The rendered region then carries *no* ``glob:`` line, because any narrower
  trigger for such a checker is precisely the bug this file eradicates.
"""

from __future__ import annotations

import argparse
import ast
import difflib
import re
import sys
from pathlib import Path

# lefthook.yml and every scripts/ci module whose INPUT_GLOBS this renderer
# reads.  The renderer is itself a gate with inputs, so its own hook trigger is
# rendered from this declaration rather than written by hand.
INPUT_GLOBS: tuple[str, ...] = ("lefthook.yml", "scripts/ci/*.py", "scripts/ci/check-rust.sh")

ROOT = Path(__file__).resolve().parents[2]
HOOKS = ROOT / "lefthook.yml"
SCRIPTS = ROOT / "scripts" / "ci"

UNIVERSAL = "**/*"
MARKER_BEGIN = "# >>> generated glob"
MARKER_END = "# <<< generated glob"
INDENT = "      "

COMMAND = re.compile(r"^    ([A-Za-z0-9][A-Za-z0-9_.-]*):[ \t]*$")
TOP_LEVEL = re.compile(r"^[A-Za-z]")
GLOB_LINE = re.compile(r"^      glob:")
INVOCATION = re.compile(r"scripts/ci/([A-Za-z0-9_]+\.py)")
PATTERN = re.compile(r"[A-Za-z0-9_./*-]+")


def gate_branch(gate: str, pattern: str) -> str:
    """One literal `case` arm of the Rust gate; shared with orchestration."""
    match = re.search(rf"(?ms)^  {re.escape(pattern)}\)\n(.*?)^    ;;", gate)
    return match.group(1) if match else ""


def gate_mode_source(gate: str, mode: str) -> str:
    """Conservative call-site view of the existing Rust shell gate, not a runner.

    Unknown ordinary conditions retain both branches. Only literal mode guards
    exclude calls. Loops retain their body; functions are inspected too. Refuse
    changes to the supported mode-control shape instead of losing a trigger.
    """
    assignments = 0
    for line in gate.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if re.search(r"\bmode(?:\+)?=", stripped):
            assignments += 1
            if stripped != 'mode="${1:---full}"' or assignments != 1:
                raise SystemExit("generate_hooks: Rust gate mode is reassigned")
        elif re.search(r"\b(?:read|unset|readonly|declare|typeset|local)\b.*\bmode\b|\bprintf\s+-v\s", stripped):
            raise SystemExit("generate_hooks: unsupported Rust gate mode write")
    if assignments != 1:
        raise SystemExit("generate_hooks: Rust gate needs its canonical CLI mode assignment")
    cases = list(re.finditer(r'(?m)^case "\$mode" in\n', gate))
    if len(cases) != 2 or re.search(r'(?m)^\s*case (?!"\$mode" in$)', gate):
        raise SystemExit("generate_hooks: unsupported Rust gate case shape")
    admission_end = gate.find("\nesac\n", cases[0].end())
    if admission_end < 0 or INVOCATION.search(gate[cases[0].end():admission_end]):
        raise SystemExit("generate_hooks: unsupported Rust gate mode admission")
    dispatch = gate[cases[-1].start():]
    dispatch_end = dispatch.find("\nesac\n")
    if dispatch_end < 0:
        raise SystemExit("generate_hooks: incomplete Rust gate mode dispatch")
    patterns = re.findall(r"(?m)^  ([^()\n]+)\)\n", dispatch)
    if any(pattern != "*" and any(not re.fullmatch(r"--[a-z-]+", arm) for arm in pattern.split("|")) for pattern in patterns):
        raise SystemExit("generate_hooks: dynamic Rust gate mode arm")
    matching = [pattern for pattern in patterns if mode in pattern.split("|")]
    if len(matching) != 1 or not gate_branch(dispatch, matching[0]):
        raise SystemExit("generate_hooks: missing or ambiguous Rust gate mode arm")
    source = (gate[:cases[0].start()] + gate[admission_end + len("\nesac\n"):cases[-1].start()]
              + gate_branch(dispatch, matching[0]) + dispatch[dispatch_end + len("\nesac\n"):])
    frames: list[bool | None] = []
    selected: list[str] = []
    for line in source.splitlines(keepends=True):
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if re.search(r'python3\s+(?:-[A-Za-z]+\s+)*["$]', line):
            raise SystemExit("generate_hooks: dynamic Rust gate Python call")
        if stripped.startswith(("eval ", "source $", 'source "$', ". $", '. "$')):
            raise SystemExit("generate_hooks: dynamic Rust gate call source")
        if stripped.startswith("if "):
            if "$mode" in stripped:
                guard = re.fullmatch(r'if \[ "\$mode" = "(--[a-z-]+)" \]( && [^;]+)?; then', stripped)
                if not guard:
                    raise SystemExit("generate_hooks: unsupported Rust gate mode guard")
                if guard.group(2) and ("||" in guard.group(2) or "$mode" in guard.group(2)):
                    raise SystemExit("generate_hooks: unsupported compound Rust gate mode guard")
                # False mode makes an AND false. Matching mode leaves an opaque
                # condition undecided, so both real then/else calls are retained.
                frames.append(False if guard.group(1) != mode else (None if guard.group(2) else True))
            else:
                frames.append(None)
        elif stripped.startswith("elif "):
            if "$mode" in stripped or not frames or frames[-1] is not None:
                raise SystemExit("generate_hooks: unsupported Rust gate elif")
        elif stripped == "else":
            if not frames:
                raise SystemExit("generate_hooks: unmatched Rust gate else")
            if frames[-1] is not None:
                frames[-1] = not frames[-1]
        elif stripped == "fi":
            if not frames:
                raise SystemExit("generate_hooks: unmatched Rust gate fi")
            frames.pop()
        elif "$mode" in stripped and (stripped.startswith(("while ", "for ")) or INVOCATION.search(line)):
            raise SystemExit("generate_hooks: dynamic Rust gate mode call")
        if False not in frames:
            selected.append(line)
    if frames:
        raise SystemExit("generate_hooks: incomplete Rust gate conditional")
    return "".join(selected)


def invoked_scripts(block: str, *, expand_rust_gate: bool = True) -> set[str]:
    """Direct Python calls plus the Rust gate's actual literal hook mode."""
    lines = block.splitlines()
    commands = []
    for index, line in enumerate(lines):
        run = re.match(r"^(\s*)run:\s*(.*)$", line)
        if not run:
            continue
        commands.append(run.group(2))
        if run.group(2) in ("|", "|-", ">", ">-"):
            for following in lines[index + 1:]:
                if following.strip() and len(following) - len(following.lstrip()) <= len(run.group(1)):
                    break
                commands.append(following)
    block = "\n".join(commands)
    # The native shell launcher transports a shell command; its own unit-test
    # inputs cannot replace that command's existing frontend/contract inputs.
    # Only the literal shell-transport form is excluded. Direct --self-test
    # calls still declare the launcher's actual Python test inputs.
    calls = re.sub(r"python3\s+scripts/ci/_native_shell\.py(?=\s+scripts/[A-Za-z0-9_./-]+\.sh(?:\s|$))",
                   "python3", block)
    scripts = set(INVOCATION.findall(calls))
    if expand_rust_gate and "scripts/ci/check-rust.sh" in block:
        modes = re.findall(r"scripts/ci/check-rust\.sh (--[a-z-]+)(?:\s|$)", block)
        if len(modes) != 1:
            raise SystemExit("generate_hooks: Rust hook needs one literal mode")
        scripts.update(INVOCATION.findall(gate_mode_source((ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8"), modes[0])))
    return scripts


def _expand_braces(pattern: str) -> list[str]:
    """Expand one level of `{a,b}` alternation (nested braces expand recursively)."""
    match = re.search(r"\{([^{}]*)\}", pattern)
    if match is None:
        return [pattern]
    expanded = []
    for alternative in match.group(1).split(","):
        expanded.extend(
            _expand_braces(pattern[: match.start()] + alternative + pattern[match.end():])
        )
    return expanded


# `glob_matches` models doublestar semantics: `**/` spans zero or more
# directories and `*` stays inside one segment. lefthook's default gobwas
# matcher disagrees on both counts -- `docs/**/*.md` never fired for
# `docs/README.md` -- so a trigger proven to cover its inputs here still skipped
# every file at the top of its tree. The matcher is therefore rendered, not
# assumed: lefthook.yml must select exactly the semantics proven here.
MATCHER_LINE = "glob_matcher: doublestar\n"
MATCHER = re.compile(r"^glob_matcher:.*\n?", re.MULTILINE)


def _with_matcher(hooks: str) -> str:
    """``hooks`` declaring the one glob matcher this renderer's proof models."""

    if MATCHER.search(hooks):
        return MATCHER.sub(lambda _: MATCHER_LINE, hooks, count=1)
    lines = hooks.splitlines(keepends=True)
    for index, line in enumerate(lines):
        if TOP_LEVEL.match(line):
            return "".join(lines[:index] + [MATCHER_LINE, "\n"] + lines[index:])
    return hooks + ("" if hooks.endswith("\n") or not hooks else "\n") + MATCHER_LINE


def glob_matches(pattern: str, path: str) -> bool:
    """Does one lefthook glob (brace alternation, `*`, `**`) match one path?"""
    for alternative in _expand_braces(pattern):
        regex = (
            re.escape(alternative)
            .replace(r"\*\*/", "(?:.*/)?")
            .replace(r"\*\*", ".*")
            .replace(r"\*", "[^/]*")
        )
        if re.fullmatch(regex, path):
            return True
    return False


def declared_input_globs(scripts: Path) -> dict[str, tuple[str, ...]]:
    """Module-level ``INPUT_GLOBS`` of every Python module in ``scripts``.

    Parsed, never imported: a declaration must be a literal tuple of strings so
    reading it cannot depend on a checker's runtime state or side effects.
    """

    declarations: dict[str, tuple[str, ...]] = {}
    for path in sorted(scripts.glob("*.py")):
        try:
            module = ast.parse(path.read_text(encoding="utf-8"))
        except SyntaxError as error:  # pragma: no cover - a broken checker fails elsewhere
            raise SystemExit(f"generate_hooks: cannot parse {path}: {error}")
        for node in module.body:
            if isinstance(node, ast.AnnAssign):
                targets = [node.target]
                value = node.value
            elif isinstance(node, ast.Assign):
                targets = node.targets
                value = node.value
            else:
                continue
            if not any(
                isinstance(target, ast.Name) and target.id == "INPUT_GLOBS"
                for target in targets
            ):
                continue
            if not isinstance(value, ast.Tuple):
                raise SystemExit(f"generate_hooks: {path.name} INPUT_GLOBS must be a tuple")
            patterns: list[str] = []
            for item in value.elts:
                if not isinstance(item, ast.Constant) or not isinstance(item.value, str):
                    raise SystemExit(
                        f"generate_hooks: {path.name} INPUT_GLOBS must hold string literals"
                    )
                if not PATTERN.fullmatch(item.value):
                    raise SystemExit(
                        f"generate_hooks: {path.name} declares an unrenderable glob "
                        f"{item.value!r} (a comma or brace would break the alternation)"
                    )
                patterns.append(item.value)
            if not patterns:
                raise SystemExit(f"generate_hooks: {path.name} INPUT_GLOBS is empty")
            declarations[path.name] = tuple(patterns)
    return declarations


def _spans(hooks: str) -> tuple[list[str], list[list]]:
    """Line list plus ``[name, header_index, end_index]`` per pre-commit command."""

    lines = hooks.splitlines(keepends=True)
    section: str | None = None
    spans: list[list] = []
    for index, line in enumerate(lines):
        if TOP_LEVEL.match(line):
            if spans and spans[-1][2] is None:
                spans[-1][2] = index
            section = line.split(":", 1)[0]
            continue
        if section != "pre-commit":
            continue
        match = COMMAND.match(line)
        if match is None:
            continue
        if spans and spans[-1][2] is None:
            spans[-1][2] = index
        spans.append([match.group(1), index, None])
    for span in spans:
        if span[2] is None:
            span[2] = len(lines)
    return lines, spans


def _body_end(lines: list[str], start: int, limit: int) -> int:
    """Trim the blank lines and indent-4 comments that introduce the next command."""

    end = limit
    while end > start and (not lines[end - 1].strip() or lines[end - 1].startswith("    #")):
        end -= 1
    return end


def _without_generated(block: list[str]) -> list[str]:
    """Drop the generated region and any glob property, keeping the rest verbatim."""

    kept: list[str] = []
    inside = False
    for line in block:
        stripped = line.strip()
        if stripped.startswith(MARKER_BEGIN):
            inside = True
            continue
        if stripped.startswith(MARKER_END):
            inside = False
            continue
        if inside or GLOB_LINE.match(line):
            continue
        kept.append(line)
    return kept


def _brace(patterns: list[str]) -> str:
    return patterns[0] if len(patterns) == 1 else "{" + ",".join(patterns) + "}"


def _region(scripts: list[str], declarations: dict[str, tuple[str, ...]]) -> list[str]:
    sources = ", ".join(scripts)
    begin = f"{INDENT}{MARKER_BEGIN} from {sources} INPUT_GLOBS — do not edit\n"
    end = f"{INDENT}{MARKER_END}\n"
    patterns = sorted({pattern for name in scripts for pattern in declarations[name]})
    if UNIVERSAL in patterns:
        return [
            begin,
            f"{INDENT}# no glob: {sources} reads every staged file, so this command\n"
            f"{INDENT}# must keep the whole staged set as its trigger.\n",
            end,
        ]
    for name in scripts:
        own = f"scripts/ci/{name}"
        if not any(glob_matches(pattern, own) for pattern in patterns):
            patterns.append(own)
    return [begin, f'{INDENT}glob: "{_brace(sorted(patterns))}"\n', end]


def wired_scripts(hooks: str) -> set[str]:
    """Checkers a pre-commit command actually runs (glob text does not count)."""

    lines, spans = _spans(hooks)
    wired: set[str] = set()
    for _, header, end in spans:
        block = lines[header + 1:_body_end(lines, header + 1, end)]
        wired.update(invoked_scripts("".join(_without_generated(block))))
    return wired


def undeclared_invocations(hooks: str, declarations: dict[str, tuple[str, ...]]) -> list[str]:
    """Checkers a ``pre-commit`` command runs that declare no ``INPUT_GLOBS``.

    A command invoking an undeclared checker keeps whatever glob a human wrote,
    and nothing afterwards compares that glob to what the checker reads. The
    drift is silent and one-directional: the checker grows a new input, the glob
    does not, and the check simply stops running for the paths it stopped being
    triggered by. That is the failure this renderer exists to remove, so leaving
    the door open for new commands is not a policy - it is the bug, deferred.

    Returning the names rather than rendering something for them keeps the
    renderer honest: it cannot invent a trigger for inputs nobody declared.
    """

    lines, spans = _spans(hooks)
    undeclared: set[str] = set()
    for _, header, end in spans:
        body_end = _body_end(lines, header + 1, end)
        block = _without_generated(lines[header + 1:body_end])
        for name in invoked_scripts("".join(block), expand_rust_gate=False):
            if name not in declarations:
                undeclared.add(name)
    return sorted(undeclared)


def render(hooks: str, declarations: dict[str, tuple[str, ...]]) -> str:
    """``hooks`` with every generated glob region rebuilt from ``declarations``."""

    lines, spans = _spans(hooks)
    output: list[str] = []
    cursor = 0
    for _, header, end in spans:
        body_end = _body_end(lines, header + 1, end)
        block = lines[header + 1:body_end]
        cleaned = _without_generated(block)
        scripts = sorted(
            {
                name
                for name in invoked_scripts("".join(cleaned))
                if name in declarations
            }
        )
        marked = any(line.strip().startswith(MARKER_BEGIN) for line in block)
        if not scripts and not marked:
            continue
        output.extend(lines[cursor:header + 1])
        output.extend(_region(scripts, declarations) if scripts else [])
        output.extend(cleaned)
        cursor = body_end
    output.extend(lines[cursor:])
    return _with_matcher("".join(output))


def selftest() -> None:
    """Cause/effect decision table, proven on a synthetic hook file.

    Causes: C1 the command runs a checker that declares INPUT_GLOBS; C2 the
    command already carries the exact generated region; C3 the command carries
    a hand-written glob outside the markers; C4 the glob inside the markers was
    hand-edited; C5 the declaration is the ``**/*`` sentinel; C6 the command
    runs two declaring checkers.

    | Rule | C1 | C2 | C3 | C4 | C5 | C6 | Effect                                    |
    |------|----|----|----|----|----|----|-------------------------------------------|
    | G1   | T  | T  | F  | F  | F  | F  | fixpoint: render leaves the file byte-equal|
    | G2   | T  | F  | T  | F  | F  | F  | the hand-written glob is replaced          |
    | G3   | T  | T  | F  | T  | F  | F  | the edit is overwritten by the declaration |
    | G4   | F  | F  | T  | F  | F  | F  | untouched: no declaring checker is invoked |
    | G5   | T  | F  | F  | F  | T  | F  | a region with no glob line at all          |
    | G6   | T  | F  | F  | F  | F  | T  | one glob, the union of both declarations   |

    Mandatory declaration (U): a rendered trigger only exists for a checker that
    declared its inputs, so a `pre-commit` command running an undeclared Python
    checker is reported rather than left with an unaudited hand-written glob. A
    command running a shell script declares nothing and is out of scope - G4
    stays untouched, which is why the rule matches `.py` invocations only.

    | Rule | Command runs                      | Effect                        |
    |------|-----------------------------------|-------------------------------|
    | U1   | an undeclared `.py` checker       | named by undeclared_invocations |
    | U2   | only declaring checkers + a shell | nothing reported              |

    Matcher (M): the proof above is only valid under doublestar semantics.

    | Rule | lefthook.yml declares       | Effect                                   |
    |------|-----------------------------|------------------------------------------|
    | M1   | no glob_matcher             | doublestar is inserted before first key  |
    | M2   | glob_matcher: <other value> | replaced by doublestar                   |
    | M3   | glob_matcher: doublestar    | unchanged (fixpoint)                     |
    | M4   | `dir/**/*.x` vs `dir/top.x` | glob_matches models the top-level match  |
    """

    declarations = {
        "check_alpha.py": ("docs/*.md",),
        "check_beta.py": ("formal/*.json",),
        "check_everything.py": (UNIVERSAL,),
    }
    hand_written = (
        "pre-commit:\n  commands:\n"
        "    alpha:\n"
        '      glob: "docs/one.md"\n'
        "      run: python3 scripts/ci/check_alpha.py {staged_files}\n"
        "\n    shell:\n"
        '      glob: "crates/**/*.rs"\n'
        "      run: tools/opaque.sh\n"
        "\n    everything:\n"
        '      glob: "docs/*.md"\n'
        "      run: python3 scripts/ci/check_everything.py --staged\n"
        "\n    both:\n"
        "      run: |\n"
        "        python3 scripts/ci/check_alpha.py\n"
        "        python3 scripts/ci/check_beta.py\n"
        "\ncommit-msg:\n  commands:\n"
        "    message:\n"
        "      run: python3 scripts/ci/check_alpha.py {1}\n"
    )
    generated = render(hand_written, declarations)
    assert render(generated, declarations) == generated, "G1 render is a fixpoint"
    assert '      glob: "{docs/*.md,scripts/ci/check_alpha.py}"\n' in generated, "G2"
    assert '      glob: "docs/one.md"\n' not in generated, "G2 drops the hand-written glob"
    edited = generated.replace('{docs/*.md,scripts/ci/check_alpha.py}', 'docs/one.md')
    assert render(edited, declarations) == generated, "G3 an in-marker edit is restored"
    shell = generated.split("    shell:\n", 1)[1].split("\n    ", 1)[0]
    assert '      glob: "crates/**/*.rs"' in shell and MARKER_BEGIN not in shell, "G4"
    everything = generated.split("    everything:\n", 1)[1].split("\n    ", 1)[0]
    assert MARKER_BEGIN in everything and "glob:" not in everything, "G5"
    assert '      glob: "{docs/*.md,formal/*.json,scripts/ci/check_alpha.py,' \
        'scripts/ci/check_beta.py}"\n' in generated, "G6"
    # The commit-msg section has no staged-file trigger set, so it is not rendered.
    assert generated.split("commit-msg:", 1)[1].count("glob:") == 0, "scope: commit-msg"
    assert wired_scripts(generated) == {
        "check_alpha.py", "check_beta.py", "check_everything.py",
    }, "wiring is read from run bodies, not from glob text"
    # U2 the fixture's own commands are all declared (its shell command runs no
    # Python checker), so the mandatory-declaration rule reports nothing.
    assert undeclared_invocations(generated, declarations) == [], "U2"
    transported = hand_written.replace("run: tools/opaque.sh",
        "run: python3 scripts/ci/_native_shell.py scripts/contract/generate-contracts.sh --check")
    assert render(transported, {**declarations, "_native_shell.py": ("scripts/ci/_native_shell.py",)}) == \
        generated.replace("run: tools/opaque.sh",
            "run: python3 scripts/ci/_native_shell.py scripts/contract/generate-contracts.sh --check"), \
        "U3 shell transport must preserve the existing command trigger"
    assert invoked_scripts("run: python3 scripts/ci/_native_shell.py --self-test") == {"_native_shell.py"}, \
        "U4 native launcher tests still own their declared inputs"
    assert invoked_scripts("run: python3 scripts/ci/_native_shell.py scripts/ci/opaque.sh && "
                           "python3 scripts/ci/check_alpha.py") == {"check_alpha.py"}, \
        "U5 a shell transport cannot hide another Python checker"
    assert invoked_scripts('      glob: "scripts/ci/_native_shell.py"\n'
                           '      # python3 scripts/ci/check_beta.py\n'
                           '      run: python3 scripts/ci/check_alpha.py\n') == {"check_alpha.py"}, \
        "U6 globs and comments cannot pretend a checker executes"
    # U1 one added command runs a checker that declared nothing.
    # Inserted inside the pre-commit commands block: a command appended past the
    # end of that section is not a pre-commit trigger and must not be scanned.
    with_undeclared = generated.replace(
        "pre-commit:\n  commands:\n",
        "pre-commit:\n  commands:\n"
        "    stray:\n"
        '      glob: "docs/*.md"\n'
        "      run: python3 scripts/ci/check_stray.py {staged_files}\n\n",
        1,
    )
    assert undeclared_invocations(with_undeclared, declarations) == [
        "check_stray.py"
    ], "U1"

    # A declaration that cannot be rendered into brace alternation is refused at
    # the source rather than emitted as a broken glob.
    for broken in ['INPUT_GLOBS = ("a,b",)\n', 'INPUT_GLOBS = ()\n', 'INPUT_GLOBS = ["a"]\n']:
        import tempfile

        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "check_broken.py").write_text(broken, encoding="utf-8")
            try:
                declared_input_globs(Path(directory))
            except SystemExit:
                continue
            raise AssertionError(f"an unrenderable declaration was accepted: {broken!r}")
    assert generated.count(MATCHER_LINE) == 1 and generated.startswith(MATCHER_LINE), "M1"
    commented = "# header\n" + hand_written
    inserted = render(commented, declarations)
    assert inserted.startswith("# header\n" + MATCHER_LINE), "M1 after leading comments"
    other = render("glob_matcher: gobwas\n" + hand_written, declarations)
    assert other.count("glob_matcher:") == 1 and MATCHER_LINE in other, "M2"
    assert render(other, declarations) == other, "M3"
    assert glob_matches("types/**/*.rs", "types/top.rs"), "M4 zero directories"
    assert glob_matches("types/**/*.rs", "types/a/b.rs"), "M4 nested"
    assert not glob_matches("docs/*.md", "docs/wiki/a.md"), "M4 * stays in one segment"
    # B-trigger causal table: T1 common prefix + nested materializer loop calls
    # -> included; T2 quick arm -> included; T3 push-only/full-only/self-test
    # early-exit calls -> excluded; T4 unknown/dynamic mode or Python call ->
    # refuse generation, never fewer inputs. T5 docs-only -> no Rust trigger;
    # T6 docs plus manifest/hook/toolchain/fuzz input -> Rust trigger and the
    # affected planner's whole fallback. This is the actual shell control shape,
    # not a second declared checker registry or a shell execution wrapper.
    gate = (ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8")
    quick = gate_mode_source(gate, "--quick")
    assert "rust_test_plan.py" in quick and "check_dependency_sources.py" in quick
    assert "check_formal_coverage.py" not in quick and "check_formal_surface.py" not in quick
    assert "gate_tools.py" not in quick and "--no-run --timings" not in quick
    early = gate.replace('mode="${1:---full}"\n', 'python3 scripts/ci/prefix_probe.py\nmode="${1:---full}"\n')
    assert "prefix_probe.py" in gate_mode_source(early, "--quick"), "T1 early common prefix must be retained"
    assert "suffix_probe.py" in gate_mode_source(gate + "\npython3 scripts/ci/suffix_probe.py\n", "--quick"), "T1 common suffix is retained"
    loop = gate.replace("for materializer_profile in bare brokered; do\n",
                        "for materializer_profile in bare brokered; do\n  python3 scripts/ci/loop_probe.py\n")
    assert "loop_probe.py" in gate_mode_source(loop, "--quick"), "T1 loop is retained"
    self_test = gate.replace('if [ "$mode" = "--self-test" ]; then\n',
                             'if [ "$mode" = "--self-test" ]; then\n  python3 scripts/ci/self_probe.py\n')
    assert "self_probe.py" not in gate_mode_source(self_test, "--quick"), "T3 early-exit mode is excluded"
    # T1 compound-mode cause: matching mode + opaque false condition takes the
    # real else branch. Both outcomes of that condition must own their inputs.
    compound = gate.replace('if [ "$mode" = "--quick" ]; then',
                            'if [ "$mode" = "--quick" ] && test -n "${OPAQUE:-}"; then')
    compound = compound.replace('    else\n      step "check"',
                                '    else\n      python3 scripts/ci/compound_else_probe.py\n      step "check"')
    assert "rust_test_plan.py" in gate_mode_source(compound, "--quick")
    assert "compound_else_probe.py" in gate_mode_source(compound, "--quick"), "T1 compound else must be retained"
    for broken in (gate.replace("  --quick|--push)\n", "  --push)\n"),
                   gate.replace('if [ "$mode" = "--quick" ]; then', 'if test "$mode" = "--quick"; then'),
                   gate.replace("python3 scripts/ci/rust_test_plan.py --base", 'python3 "$checker" --base'),
                   gate.replace("python3 scripts/ci/rust_test_plan.py --base", 'eval "$checker" --base'),
                   gate.replace('mode="${1:---full}"\n', 'mode="${1:---full}"\nmode="--push"\n'),
                   gate.replace('mode="${1:---full}"\n', 'mode="${1:---full}"\nreadonly mode="--push"\n'),
                   gate.replace("  --full)\n", '  "$dynamic")\n')):
        try:
            gate_mode_source(broken, "--quick")
        except SystemExit:
            pass
        else:
            raise AssertionError("T4 unsupported control shape accepted")
    inputs = declared_input_globs(SCRIPTS)["rust_test_plan.py"]
    assert not any(glob_matches(pattern, "docs/design.md") for pattern in inputs), "T5 docs fastpath"
    for path in ("Cargo.toml", "crates/contract/foo/Cargo.toml", "lefthook.yml", "rust-toolchain.toml",
                 ".cargo/config.toml", ".config/rust-tests.toml", "fuzz/fuzz_targets/foo.rs", "scripts/ci/check-rust.sh"):
        assert any(glob_matches(pattern, path) for pattern in inputs), f"T6 uncovered {path}"
    print("generate_hooks self-test: generated globs follow their declarations")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail when lefthook.yml is stale, printing the diff",
    )
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    args = parser.parse_args(argv)

    if args.self_test:
        selftest()
        return 0

    current = HOOKS.read_text(encoding="utf-8")
    declarations = declared_input_globs(SCRIPTS)
    undeclared = undeclared_invocations(current, declarations)
    if undeclared:
        print(
            "generate_hooks: these pre-commit checkers declare no INPUT_GLOBS, so their "
            "trigger stays hand-written and can drift from what they read:\n  "
            + "\n  ".join(undeclared)
            + "\nAdd a module-level INPUT_GLOBS tuple naming every path the checker reads.",
            file=sys.stderr,
        )
        return 1
    rendered = render(current, declarations)
    if rendered == current:
        print("lefthook.yml generated globs are current")
        return 0
    if args.check:
        sys.stdout.writelines(
            difflib.unified_diff(
                current.splitlines(keepends=True),
                rendered.splitlines(keepends=True),
                fromfile="lefthook.yml",
                tofile="lefthook.yml (generated)",
            )
        )
        print(
            "\nlefthook.yml is stale; run: python3 scripts/ci/generate_hooks.py",
            file=sys.stderr,
        )
        return 1
    HOOKS.write_text(rendered, encoding="utf-8")
    print("lefthook.yml generated globs updated")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
