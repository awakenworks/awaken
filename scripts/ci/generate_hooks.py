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

* Only the ``pre-commit`` inventory is rendered. ``commit-checks`` is its one
  authored definition; a transient child view retains the native hook name so
  Lefthook still owns filtering and automatic staging. ``commit-msg``/``pre-push``
  commands have no staged-file trigger set, so there is nothing to narrow: the
  commit-message validator takes the message file as ``{1}``, and the push gate
  runs unconditionally.
* Generated globs and observation transport surround the original checker body.
  Command names, bodies, read-only grouping and comments stay human-authored.
  The renderer decodes that same body for every composition/input reader; no
  second command inventory or filter is maintained. The Rust shell's literal mode
  arms and guards select its Python input owners; unsupported control shapes
  refuse generation. Other shell scripts and non-Python tools stay untouched.
* The invoked checker's own path, and the path of every ``scripts/ci`` module
  it imports (transitively, lazy imports included), are added to its rendered
  glob automatically (unless an already-declared pattern matches them),
  because editing a checker or a helper it runs changes what it accepts.
  ``trigger_patterns`` is that one trigger; ``_input_closure.py`` holds each
  checker's reads to the same value.
* ``INPUT_GLOBS = ("**/*",)`` is the sentinel for "reads every staged file".
  The rendered region then carries *no* ``glob:`` line, because any narrower
  trigger for such a checker is precisely the bug this file eradicates.
"""

from __future__ import annotations

import argparse
import ast
import difflib
import functools
import json
import re
import shlex
import sys
from pathlib import Path, PurePosixPath

# lefthook.yml and every scripts/ci module whose INPUT_GLOBS this renderer
# reads.  The renderer is itself a gate with inputs, so its own hook trigger is
# rendered from this declaration rather than written by hand.
INPUT_GLOBS: tuple[str, ...] = ("lefthook.yml", "scripts/ci/*.py", "scripts/ci/*.sh",
                                 "scripts/ci/pre-commit/*.sh", "scripts/ci/pre-push/*.sh")

ROOT = Path(__file__).resolve().parents[2]
HOOKS = ROOT / "lefthook.yml"
SCRIPTS = ROOT / "scripts" / "ci"

UNIVERSAL = "**/*"
MARKER_BEGIN = "# >>> generated glob"
MARKER_END = "# <<< generated glob"
INDENT = "      "

COMMAND = re.compile(r"^    ([A-Za-z0-9][A-Za-z0-9_.-]*):[ \t]*$")
TOP_LEVEL = re.compile(r"^[A-Za-z]")
GLOB_LINE = re.compile(r"^\s+glob:")
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
    block = hook_run(block)
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
        source = gate_mode_source((ROOT / "scripts/ci/check-rust.sh").read_text(encoding="utf-8"), modes[0])
        scripts.update(INVOCATION.findall(source))
        # The gate itself and the shell helpers this mode sources or runs: a
        # change to them changes what the gate does, like a Python callee.
        scripts.add("check-rust.sh")
        scripts.update(rust_gate_shell_callees(source))
    return scripts


def rust_gate_shell_callees(source: str) -> set[str]:
    """Shell files one Rust-gate mode source sources or runs (not ones it only reads)."""
    callees = set()
    for line in source.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or stripped.split()[0] in SHELL_DATA_COMMANDS:
            continue
        callees.update(name for name in re.findall(r"scripts/ci/([A-Za-z0-9_.-]+\.sh)\b", stripped)
                       if (SCRIPTS / name).is_file())
    return callees


def hook_run(block: str, *, unwrap=True) -> str:
    """The actual checker body, independent of the observation transport."""
    lines = block.splitlines()
    commands = []
    for index, line in enumerate(lines):
        run = re.match(r"^(\s*)run:\s*(.*)$", line)
        if not run:
            continue
        value = run.group(2)
        if value in ("|", "|-", ">", ">-"):
            style = value
            following_lines = []
            for following in lines[index + 1:]:
                if following.strip() and len(following) - len(following.lstrip()) <= len(run.group(1)):
                    break
                following_lines.append(following[len(run.group(1)) + 2:])
            value = ('\n'.join(following_lines).rstrip('\n') + ('\n' if style == '|' else '')
                     if style.startswith('|') else ' '.join(line.strip() for line in following_lines))
        if value.startswith('"'):
            value = json.loads(value)  # JSON strings are an unambiguous YAML scalar subset.
        elif value.startswith("'") and value.endswith("'"):
            value = value[1:-1].replace("''", "'")
        prefix = "python3 scripts/ci/parallel_check.py --hook-label "
        if unwrap and value.startswith(prefix):
            from parallel_check import parser
            words = shlex.split(value)
            args = parser().parse_args(words[2:])
            tail = args.command[1:] if args.command[:1] == ['--'] else args.command
            if not args.hook_label or not args.shell or tail not in ([], ['{staged_files}']):
                raise SystemExit('generate_hooks: unsupported hook observation transport')
            value = args.shell
            if tail:
                if value.count('"$@"') != 1:
                    raise SystemExit('generate_hooks: staged argument transport must occur once')
                value = value.replace('"$@"', '{staged_files}')
        commands.append(value)
    return "\n".join(commands)


def observed_hook_run(name: str, body: str, *, readonly: bool, timeout=None) -> str:
    """Render the adapter; Lefthook alone selects and quotes staged filenames."""
    uses_files = '{staged_files}' in body
    if body.count('{staged_files}') > 1:
        raise SystemExit('generate_hooks: staged template must occur at most once')
    command = ('python3 scripts/ci/parallel_check.py --hook-label ' + name
               + ' --timeout ' + str(timeout if timeout is not None else 600 if readonly else 14400)
               + (' --read-only' if readonly else '')
               + ' --shell ' + shlex.quote(body.replace('{staged_files}', '"$@"'))
               + ' --' + (' {staged_files}' if uses_files else ''))
    return json.dumps(command) if '\n' in command or ': ' in command else command


def _observed_body(name, body):
    text = ''.join(body)
    readonly = bool(re.search(r'(?m)^\s+tags: \[read-only\]$', text))
    raw = hook_run(text, unwrap=False)
    timeout = None
    if raw.startswith('python3 scripts/ci/parallel_check.py --hook-label '):
        from parallel_check import parser
        timeout = parser().parse_args(shlex.split(raw)[2:]).timeout
    value = observed_hook_run(name, hook_run(text), readonly=readonly, timeout=timeout)
    return _replace_hook_run(body, value)


def _replace_hook_run(body, value):
    """Replace only run/its YAML continuation, retaining every later option."""
    output, in_literal_body, continuation_indent = [], False, ''
    for line in body:
        match = re.match(r'^(\s*)run:', line)
        if match:
            if '\n' in value:
                output.append(match[1] + ('run: |\n' if value.endswith('\n') else 'run: |-\n'))
                output.extend(match[1] + '  ' + part + '\n' for part in value.splitlines())
            else:
                output.append(match[1] + 'run: ' + value + '\n')
            in_literal_body = line.strip() in ('run: |', 'run: |-', 'run: >', 'run: >-')
            continuation_indent = match[1] + '  '
        elif in_literal_body and (not line.strip() or line.startswith(continuation_indent)):
            continue
        else:
            in_literal_body = False
            output.append(line)
    return output


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
        declared = input_globs_of(path)
        if declared is not None:
            declarations[path.name] = declared
    return declarations


def input_globs_of(path: Path) -> tuple[str, ...] | None:
    """One module's literal ``INPUT_GLOBS``, or None when it declares none."""
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
        return tuple(patterns)
    return None


def _hook_nodes(hooks: str):
    """One reader for authored groups and leaves, with original source spans."""

    lines = hooks.splitlines(keepends=True)
    section: str | None = None
    sections = {'commit-checks'} if re.search(r'(?m)^commit-checks:', hooks) else {'pre-commit'}
    headers = []
    for index, line in enumerate(lines):
        if TOP_LEVEL.match(line):
            section = line.split(":", 1)[0]
            continue
        if section not in sections:
            continue
        match = re.fullmatch(r'(\s*)- name: ([A-Za-z0-9_.-]+)\s*', line)
        if re.match(r'\s+- (?:name|run|script|group):', line) and not match:
            raise SystemExit('generate_hooks: hook jobs require one literal, named source node')
        old = COMMAND.match(line) if section == 'pre-commit' else None
        if match or old:
            headers.append((match.group(2) if match else old.group(1), index,
                            len(match.group(1)) if match else 4))
    nodes = []
    for name, start, indent in headers:
        end = next((index for index in range(start + 1, len(lines))
                    if TOP_LEVEL.match(lines[index]) or (lines[index].strip()
                       and not lines[index].lstrip().startswith('#')
                       and len(lines[index]) - len(lines[index].lstrip()) <= indent)), len(lines))
        body = ''.join(lines[start + 1:end])
        group = bool(re.search(r'(?m)^' + ' ' * (indent + 2) + r'group:', body))
        nodes.append([name, start, end, indent, group])
    if len({node[0] for node in nodes}) != len(nodes):
        raise SystemExit('generate_hooks: duplicate pre-commit job name')
    return lines, nodes


def _spans(hooks: str) -> tuple[list[str], list[list]]:
    lines, nodes = _hook_nodes(hooks)
    spans = [[name, start, end] for name, start, end, _, group in nodes if not group]
    return lines, spans


def hook_execution_errors(hooks: str) -> list[str]:
    """Native Lefthook executes one bounded view of the existing job inventory."""
    section_name = 'commit-checks' if 'commit-checks:' in hooks else 'pre-commit'
    section = hooks.split(section_name + ':\n', 1)[-1].split('\ncommit-msg:', 1)[0]
    if not re.search(r'(?m)^\s+- name:', section):
        return []  # Read-only parser fixtures also cover the original flat syntax.
    from parallel_check import parser
    lines, nodes = _hook_nodes(hooks)
    errors = []
    direct = [line.strip() for line in section.splitlines() if re.match(r'^  \S', line)]
    if direct != ['parallel: false', 'piped: true', 'follow: true', 'jobs:']:
        errors.append('pre-commit must keep sequential barriers and live worker output')
    by_start = {node[1]: node for node in nodes}
    parents = {}
    children = {None: []}
    for node in nodes:
        candidates = [other for other in nodes if other[4] and other[1] < node[1] < other[2]]
        parent = max(candidates, key=lambda item: item[1])[1] if candidates else None
        parents[node[1]] = parent
        children.setdefault(parent, []).append(node)
    modes = {}
    for name, start, end, indent, group in nodes:
        body = ''.join(lines[start + 1:end])
        parent = parents[start]
        expected_indent = by_start[parent][3] + 6 if parent is not None else 4
        if indent != expected_indent:
            errors.append('unsupported hook nesting: ' + name)
        if group:
            direct_children = children.get(start, [])
            prefix = ''.join(lines[start + 1:direct_children[0][1]]) if direct_children else body
            settings = [line.strip() for line in prefix.splitlines()
                        if line.strip() and not line.lstrip().startswith('#')]
            if settings == ['group:', 'parallel: true', 'jobs:']:
                modes[start] = 'parallel'
            elif settings == ['group:', 'piped: true', 'jobs:']:
                modes[start] = 'serial'
            else:
                errors.append('invalid bounded read-only group: ' + name)
            if not direct_children:
                errors.append('empty hook group: ' + name)
    def width(node):
        if not node[4]:
            return 1
        widths = [width(child) for child in children.get(node[1], [])]
        return sum(widths) if modes.get(node[1]) == 'parallel' else max(widths, default=0)
    if any(width(node) > 4 for node in children.get(None, [])):
        errors.append('hook execution may exceed four workers')
    for name, start, end, indent, group in nodes:
        if group:
            continue
        body = ''.join(lines[start + 1:end])
        ancestor, parallel_parent = parents[start], False
        while ancestor is not None:
            parallel_parent |= modes.get(ancestor) == 'parallel'
            ancestor = parents[ancestor]
        raw = hook_run(body, unwrap=False)
        try:
            properties = re.findall(r'(?m)^' + ' ' * (indent + 2) + r'([a-z_]+):', body)
            if len(properties) != len(set(properties)) or set(properties) - {'run', 'glob', 'tags', 'stage_fixed'}:
                raise ValueError('unsupported or duplicate job option')
            if not raw.startswith('python3 scripts/ci/parallel_check.py --hook-label '):
                raise ValueError('missing observation transport')
            args = parser().parse_args(shlex.split(raw)[2:])
            readonly = bool(re.search(r'(?m)^\s+tags: \[read-only\]$', body))
            if (args.hook_label != name or args.read_only != readonly or args.timeout <= 0
                    or (parallel_parent and not readonly) or (readonly and 'stage_fixed:' in body)):
                raise ValueError('invalid label, readonly declaration, timeout or staging writer')
            hook_run(body)  # The same decoder proves exact staged argv transport.
        except (ValueError, SystemExit) as error:
            errors.append('invalid hook worker ' + name + ': ' + str(error))
    return errors


def _body_end(lines: list[str], start: int, limit: int) -> int:
    """Trim the blank lines and indent-4 comments that introduce the next command."""

    end = limit
    indent = len(lines[start - 1]) - len(lines[start - 1].lstrip())
    while end > start and (not lines[end - 1].strip() or (
            lines[end - 1].lstrip().startswith('#')
            and len(lines[end - 1]) - len(lines[end - 1].lstrip()) <= indent)):
        end -= 1
    return end


def command_blocks(hooks: str) -> dict[str, str]:
    """One logical flat view for all trigger, audit and composition readers."""
    lines, spans = _spans(hooks)
    blocks = {}
    for name, start, end in spans:
        indent = len(lines[start]) - len(lines[start].lstrip()) + 2
        body = lines[start + 1:_body_end(lines, start + 1, end)]
        normalized = [' ' * 6 + line[indent:] if line.strip() else line for line in body]
        text = ''.join(_replace_hook_run(normalized, hook_run(''.join(normalized))))
        blocks[name] = f'    {name}:\n' + text
    return blocks


def pre_commit_view(hooks: str) -> str:
    """Compatibility of the source reader, not a second execution path."""
    if not any('- name:' in line for line in hooks.splitlines()):
        return hooks
    start = hooks.index('pre-commit:')
    end = hooks.index('\ncommit-msg:', start)
    return hooks[:start] + 'pre-commit:\n  commands:\n' + ''.join(command_blocks(hooks).values()) + hooks[end:]


def native_pre_commit_configuration(hooks: str) -> str:
    """Project the one source inventory to the native pre-commit hook name.

    File filtering, stage_fixed, local config and extensions remain Lefthook's
    responsibility. The bridge is absent only in this transient child view.
    """
    hooks = hooks.replace('\r\n', '\n')
    entry = re.search(r'(?ms)^pre-commit:\n(.*?)(?=^\S|\Z)', hooks)
    checks = re.search(r'(?ms)^commit-checks:\n(.*?)(?=^\S|\Z)', hooks)
    if observation_entry_errors(hooks) or not entry or not checks or checks.start() < entry.end():
        raise ValueError('missing canonical pre-commit inventory or observation bridge')
    if hook_execution_errors(hooks):
        raise ValueError('unsafe pre-commit execution structure')
    return (hooks[:entry.start()] + 'pre-commit:\n' + checks[1]
            + hooks[entry.end():checks.start()] + hooks[checks.end():])


def observation_entry_errors(hooks: str) -> list[str]:
    entry = re.search(r'(?ms)^pre-commit:\n(.*?)(?=^\S|\Z)', hooks)
    lines = [line.strip() for line in entry[1].splitlines()
             if line.strip() and not line.lstrip().startswith('#')] if entry else []
    if (lines != ['follow: true', 'scripts:', 'observe.sh:', 'runner: python3 scripts/ci/_native_shell.py', 'args: --observe']
            or len(re.findall(r'(?m)^pre-commit:', hooks)) != 1
            or len(re.findall(r'(?m)^commit-checks:', hooks)) != 1):
        return ['pre-commit must retain its mandatory observation script and single command inventory']
    return []


def hook_entry_errors(hooks: str) -> list[str]:
    """Pre-commit structure plus the mandatory pre-push bootstrap and qualification."""
    errors = hook_execution_errors(hooks) + observation_entry_errors(hooks)
    bridge = re.search(r"(?ms)^pre-push:\n(.*?)(?=^\S|\Z)", hooks)
    lines = [line.strip() for line in bridge.group(1).splitlines()
             if line.strip() and not line.lstrip().startswith("#")] if bridge else []
    qualification = re.search(r"(?ms)^push-qualification:\n(.*?)(?=^\S|\Z)", hooks)
    commands = re.findall(r"(?m)^      run: (.+)$", qualification.group(1)) if qualification else []
    required = ["python3 scripts/ci/_native_shell.py " + command for command in (
        "scripts/ci/check-docs.sh", "scripts/ci/check-rust.sh --push",
        "scripts/ci/check_public_api.sh --require-tools --affected-upstream",
        "scripts/ci/check-frontend.sh --quick")]
    if (re.findall(r"(?m)^source_dir:\s*(\S+)\s*$", hooks) != ["scripts/ci"]
            or lines != ["scripts:", "qualify.sh:", "runner: python3 scripts/ci/_native_shell.py", "args: --qualify"]
            or len(re.findall(r"(?m)^push-qualification:$", hooks)) != 1 or commands != required):
        errors.append("pre-push must use the mandatory script bootstrap and its independent qualification hook")
    return errors


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


def local_imports(name: str, scripts: Path = SCRIPTS) -> list[str]:
    """``scripts`` modules ``name`` imports, transitively, as file names.

    Every ``import`` statement counts, including one inside a function: a lazy
    import is still code the checker runs. Parsed, never imported.
    """

    found: set[str] = set()
    pending = [name]
    while pending:
        path = scripts / pending.pop()
        if not path.is_file():
            continue
        for node in ast.walk(ast.parse(path.read_text(encoding="utf-8"))):
            if isinstance(node, ast.Import):
                modules = [alias.name.split(".")[0] for alias in node.names]
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                modules = [node.module.split(".")[0]]
            else:
                continue
            for module in modules:
                imported = f"{module}.py"
                if imported != name and imported not in found and (scripts / imported).is_file():
                    found.add(imported)
                    pending.append(imported)
    return sorted(found)


# A `scripts/ci` path named in a shell line, literally or through the script's
# own directory variable (`$script_dir/x.sh`, `"$deadline_dir/deadline.py"`).
SHELL_REFERENCE = re.compile(
    r"(?:scripts/ci/|\$\{?[A-Za-z_]*dir\}?\"?/)((?:pre-commit/|pre-push/)?[A-Za-z0-9_.-]+\.(?:sh|py))\b")
# Shell commands that only read, print or compare the file they name: the
# reference is data (the file itself), not code that runs its own callees.
SHELL_DATA_COMMANDS = frozenset({
    "grep", "sed", "awk", "cat", "echo", "printf", "cmp", "diff", "test", "[", "[[", "rm", "cp",
    "ln", "mv", "touch", "head", "tail", "wc", "chmod", "stat", "readlink", "dirname", "basename",
    "git", "sha256sum", "local", "usage"})
PYTHON_REFERENCE = re.compile(r"(?:^|/)((?:pre-commit/|pre-push/)?[A-Za-z0-9_.-]+\.(?:sh|py))$")


SUBPROCESS_CALLS = frozenset({"run", "Popen", "call", "check_call", "check_output", "execv", "execvp",
                              "execve", "spawnv", "run_command", "_run"})


@functools.lru_cache(maxsize=None)
def script_edges(name: str, scripts: Path = SCRIPTS) -> tuple[frozenset[str], frozenset[str], frozenset[str]]:
    """``(runs, reads, mentions)``: the ``scripts`` files one script references.

    *runs* is code that executes when ``name`` does: a Python ``import`` (lazy
    ones included), a script path inside a subprocess call, and in shell a
    ``source``/``.`` or a command line naming the file. *reads* is a shell line
    whose command only reads, prints or compares the file (grep, cat, echo...).
    *mentions* is any other Python string naming a file: a test's subject or
    a checker's data, which its own INPUT_GLOBS (held by _input_closure.py at
    run time) must declare. Parsed, never run; comments/docstrings do not count.
    """

    path = scripts / name
    empty = frozenset()
    if not path.is_file():
        return empty, empty, empty
    text = path.read_text(encoding="utf-8", errors="replace")
    runs: set[str] = set()
    reads: set[str] = set()
    mentions: set[str] = set()
    if name.endswith(".sh"):
        parent = PurePosixPath(name).parent
        for line in text.splitlines():
            stripped = line.strip()
            if not stripped or stripped.startswith("#"):
                continue
            first = stripped.split()[0].lstrip("({!")
            for match in SHELL_REFERENCE.finditer(stripped):
                target = match.group(1)
                if not match.group(0).startswith("scripts/ci/"):
                    target = (parent / target).as_posix()
                data = first in SHELL_DATA_COMMANDS or stripped[:match.start()].rstrip().endswith(("<", ">"))
                (reads if data else runs).add(target)
    elif name.endswith(".py"):
        try:
            module = ast.parse(text)
        except SyntaxError:
            return empty, empty, empty
        documentation = {
            id(node.body[0].value) for node in ast.walk(module)
            if isinstance(node, (ast.Module, ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef))
            and node.body and isinstance(node.body[0], ast.Expr) and isinstance(node.body[0].value, ast.Constant)}

        def named(node: ast.AST) -> str | None:
            if isinstance(node, ast.Constant) and isinstance(node.value, str) and len(node.value) < 200:
                match = PYTHON_REFERENCE.search(node.value.strip())
                return match.group(1) if match else None
            return None

        for node in ast.walk(module):
            if isinstance(node, ast.Import):
                runs.update(f"{alias.name.split('.')[0]}.py" for alias in node.names)
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                runs.add(f"{node.module.split('.')[0]}.py")
            elif isinstance(node, ast.Call):
                function = node.func.attr if isinstance(node.func, ast.Attribute) else getattr(node.func, "id", "")
                if function in SUBPROCESS_CALLS:
                    runs.update(found for child in ast.walk(node) if (found := named(child)))
            elif id(node) not in documentation and (found := named(node)):
                mentions.add(found)
    exists = lambda found: {item for item in found if item != name and (scripts / item).is_file()}
    runs = exists(runs)
    reads = exists(reads) - runs
    return frozenset(runs), frozenset(reads), frozenset(exists(mentions) - runs - reads)


def script_closure(name: str, scripts: Path = SCRIPTS, *, mentions: bool = False) -> list[str]:
    """Every ``scripts`` file ``name`` executes (transitively) or reads.

    Executed files are followed further; a file that is only read (or, with
    ``mentions``, named by a Python string) contributes itself. This is the
    input closure of running ``name``: Python imports, shell ``source`` and
    shell callees, which ``local_imports`` alone missed, so hooks over shell
    gates used to widen to ``scripts/ci/**`` instead.
    """

    found: set[str] = set()
    pending = [name]
    expanded: set[str] = set()
    while pending:
        current = pending.pop()
        if current in expanded:
            continue
        expanded.add(current)
        runs, reads, named = script_edges(current, scripts)
        found |= runs | reads | (named if mentions else set())
        pending.extend(runs - expanded)
    found.discard(name)
    return sorted(found)


def trigger_patterns(name: str, declarations: dict[str, tuple[str, ...]],
                     scripts: Path = SCRIPTS) -> list[str]:
    """The trigger of one checker: its declaration, itself and what it runs or reads.

    A shell script needs no declaration: its trigger is itself and its closure.
    """

    patterns = sorted(set(declarations.get(name, ())))
    if UNIVERSAL in patterns:
        return [UNIVERSAL]
    # `scripts` selects the bytes to inspect, including a copied fixture. The
    # hook's authored coordinate remains scripts/ci in that repository; a
    # fixture nested under this checkout must not inherit its temporary prefix.
    prefix = "scripts/ci"
    for module in [name, *script_closure(name, scripts)]:
        own = f"{prefix}/{module}"
        if not any(glob_matches(pattern, own) for pattern in patterns):
            patterns.append(own)
    return sorted(patterns)


def _brace(patterns: list[str]) -> str:
    return patterns[0] if len(patterns) == 1 else "{" + ",".join(patterns) + "}"


def _region(scripts: list[str], declarations: dict[str, tuple[str, ...]]) -> list[str]:
    sources = ", ".join(scripts)
    begin = f"{INDENT}{MARKER_BEGIN} from {sources} INPUT_GLOBS — do not edit\n"
    end = f"{INDENT}{MARKER_END}\n"
    patterns = sorted({pattern for name in scripts for pattern in trigger_patterns(name, declarations)})
    if UNIVERSAL in patterns:
        return [
            begin,
            f"{INDENT}# no glob: {sources} reads every staged file, so this command\n"
            f"{INDENT}# must keep the whole staged set as its trigger.\n",
            end,
        ]
    # A pattern another script's trigger already covers is not repeated.
    kept = [pattern for pattern in patterns
            if not any(other != pattern and "*" in other and glob_matches(other, pattern) for other in patterns)]
    return [begin, f'{INDENT}glob: "{_brace(kept)}"\n', end]


def wired_scripts(hooks: str) -> set[str]:
    """Checkers a pre-commit command actually runs (glob text does not count)."""

    lines, spans = _spans(hooks)
    wired: set[str] = set()
    for _, header, end in spans:
        block = lines[header + 1:_body_end(lines, header + 1, end)]
        wired.update(invoked_scripts("".join(_without_generated(block))))
    return wired


def generated_hook_glob_errors(hooks: str, declarations: dict[str, tuple[str, ...]]) -> list[str]:
    """One generated-trigger/wiring model for preflight and graph validation.

    N1 rendered regions + every declaring checker wired => accept;
    N2 stale/hand-edited regions => reject; N3 unwired declaration => reject.
    The orchestration decision table exercises these same rules.
    """
    errors = []
    if render(hooks, declarations) != hooks:
        errors.append('lefthook.yml trigger globs are hand-edited or stale; regenerate them with '
                      'python3 scripts/ci/generate_hooks.py')
    for script in sorted(set(declarations) - wired_scripts(hooks)):
        errors.append(f'scripts/ci/{script} declares INPUT_GLOBS but no lefthook pre-commit '
                      'command runs it, so the declaration triggers nothing')
    return errors


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
            if name not in declarations and not name.endswith(".sh"):
                undeclared.add(name)
    return sorted(undeclared)


def render(hooks: str, declarations: dict[str, tuple[str, ...]]) -> str:
    """``hooks`` with every generated glob region rebuilt from ``declarations``."""

    lines, spans = _spans(hooks)
    output: list[str] = []
    cursor = 0
    native = any('- name:' in lines[start] for _, start, _ in spans)
    for name, header, end in spans:
        body_end = _body_end(lines, header + 1, end)
        block = lines[header + 1:body_end]
        cleaned = _without_generated(block)
        scripts = sorted(
            {
                name
                # Direct Python calls are declared (undeclared_invocations);
                # the Rust gate's own Python and shell callees need none.
                for name in invoked_scripts("".join(cleaned))
                if name in declarations or (SCRIPTS / name).is_file()
            }
        )
        marked = any(line.strip().startswith(MARKER_BEGIN) for line in block)
        if not scripts and not marked and not native:
            continue
        output.extend(lines[cursor:header + 1])
        indent = len(lines[header]) - len(lines[header].lstrip()) + 2
        region = [line for chunk in _region(scripts, declarations) for line in chunk.splitlines(keepends=True)] if scripts else []
        selected = [(' ' * indent + line[6:]) for line in region] + cleaned if scripts or marked else block
        output.extend(_observed_body(name, selected) if native else selected)
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
    # I-trigger: I1 a checker importing a helper (directly, or lazily inside a
    # function) gets the helper in its trigger; I2 transitively; I3 a module
    # outside scripts/ci (stdlib) is not a trigger; I4 a declared pattern that
    # already covers a helper is not repeated.
    with tempfile.TemporaryDirectory() as directory:
        scripts = Path(directory)
        (scripts / "check_gamma.py").write_text(
            "import json\nimport _helper\ndef f():\n    from _lazy import x\n", encoding="utf-8")
        (scripts / "_helper.py").write_text("import _deep\n", encoding="utf-8")
        (scripts / "_lazy.py").write_text("x = 1\n", encoding="utf-8")
        (scripts / "_deep.py").write_text("import check_gamma\n", encoding="utf-8")
        assert local_imports("check_gamma.py", scripts) == ["_deep.py", "_helper.py", "_lazy.py"], "I1-I3"
        assert trigger_patterns("check_gamma.py", {"check_gamma.py": ("docs/*.md",)}, scripts) == [
            "docs/*.md", "scripts/ci/_deep.py", "scripts/ci/_helper.py", "scripts/ci/_lazy.py",
            "scripts/ci/check_gamma.py"], "I1-I3 trigger"
        assert trigger_patterns("check_gamma.py", {"check_gamma.py": ("scripts/ci/*.py",)}, scripts) == [
            "scripts/ci/*.py"], "I4"
        # I5-I6 directory relocation cannot change the authored hook coordinate:
        # inside the generator checkout / outside it -> identical own/helper
        # paths, with narrow declarations retained. The former conditional
        # prefix failed I5 when TMPDIR lived under a worktree's target directory.
        from unittest.mock import patch
        expected = ["docs/*.md", "scripts/ci/_deep.py", "scripts/ci/_helper.py",
                    "scripts/ci/_lazy.py", "scripts/ci/check_gamma.py"]
        for owner in (scripts.parent, scripts / "unrelated-checkout"):
            with patch.dict(globals(), ROOT=owner):
                assert trigger_patterns("check_gamma.py", {"check_gamma.py": ("docs/*.md",)}, scripts) == expected, "I5-I6"
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
    assert "--no-run --timings" not in quick
    early = gate.replace('mode="${1:---full}"\n', 'python3 scripts/ci/prefix_probe.py\nmode="${1:---full}"\n')
    assert "prefix_probe.py" in gate_mode_source(early, "--quick"), "T1 early common prefix must be retained"
    assert "suffix_probe.py" in gate_mode_source(gate + "\npython3 scripts/ci/suffix_probe.py\n", "--quick"), "T1 common suffix is retained"
    # Each probe is injected at a fixed anchor and must actually land there, so a
    # gate edit that removes the anchor fails here instead of passing vacuously.
    loop = gate.replace('mode="${1:---full}"\n',
                        'mode="${1:---full}"\nfor probe in one two; do\n  python3 scripts/ci/loop_probe.py\ndone\n')
    assert loop != gate and "loop_probe.py" in gate_mode_source(loop, "--quick"), "T1 loop is retained"
    self_test = gate.replace('if [ "$mode" = "--self-test" ]; then\n',
                             'if [ "$mode" = "--self-test" ]; then\n  python3 scripts/ci/self_probe.py\n')
    assert self_test != gate, "T3 fixture anchor is missing"
    assert "self_probe.py" not in gate_mode_source(self_test, "--quick"), "T3 early-exit mode is excluded"
    # T1 compound-mode cause: matching mode + opaque false condition takes the
    # real else branch. Both outcomes of that condition must own their inputs.
    compound = gate.replace('  if [ "$mode" = "--push" ]; then\n    # Without',
                            '  if [ "$mode" = "--quick" ] && test -n "${OPAQUE:-}"; then\n    # Without')
    compound = compound.replace('  else\n    scope_base=HEAD',
                                '  else\n    python3 scripts/ci/compound_else_probe.py\n    scope_base=HEAD')
    assert "rust_test_plan.py" in gate_mode_source(compound, "--quick")
    assert "compound_else_probe.py" in gate_mode_source(compound, "--quick"), "T1 compound else must be retained"
    for broken in (gate.replace("  --quick|--push)\n", "  --push)\n"),
                   gate.replace('if [ "$mode" = "--full" ]; then', 'if test "$mode" = "--full"; then'),
                   gate.replace("  python3 scripts/ci/check_dependency_sources.py", '  python3 "$checker"'),
                   gate.replace("  python3 scripts/ci/check_dependency_sources.py", '  eval "$checker"'),
                   gate.replace('mode="${1:---full}"\n', 'mode="${1:---full}"\nmode="--push"\n'),
                   gate.replace('mode="${1:---full}"\n', 'mode="${1:---full}"\nreadonly mode="--push"\n'),
                   gate.replace("  --full)\n", '  "$dynamic")\n')):
        try:
            gate_mode_source(broken, "--quick")
        except SystemExit:
            pass
        else:
            raise AssertionError("T4 unsupported control shape accepted")
    # T5/T6 over the rendered rust-quick trigger itself: Rust build inputs and
    # the gate's executed closure (Python and shell callees) fire it; docs and
    # a CI script the gate never runs (the formal runner and its tests) do not.
    declarations = declared_input_globs(SCRIPTS)
    inputs = [pattern for name in invoked_scripts("run: scripts/ci/check-rust.sh --quick")
              for pattern in trigger_patterns(name, declarations)]
    for path in ("docs/design.md", "scripts/ci/check_formal.sh", "scripts/ci/test_formal_run_evidence.py"):
        assert not any(glob_matches(pattern, path) for pattern in inputs), f"T5 {path} starts the Rust gate"
    for path in ("Cargo.toml", "crates/contract/foo/Cargo.toml", "rust-toolchain.toml", ".cargo/config.toml",
                 ".config/rust-tests.toml", "fuzz/fuzz_targets/foo.rs", "scripts/ci/check-rust.sh",
                 "scripts/ci/_cargo_target.sh", "scripts/ci/_rust_test_execution.sh",
                 "scripts/ci/nextest_execution.py", "scripts/ci/check_dependency_sources.py"):
        assert any(glob_matches(pattern, path) for pattern in inputs), f"T6 uncovered {path}"
    # S-trigger: shell callees. S1 `source`/command lines run a file and are
    # followed; S2 a read-only command names data, which is not followed; S3 a
    # Python subprocess call runs its script; S4 another Python string is only
    # a mention (a test subject), not part of the trigger closure.
    with tempfile.TemporaryDirectory() as directory:
        scripts = Path(directory)
        (scripts / "gate.sh").write_text(
            "# python3 scripts/ci/commented.py\nsource scripts/ci/_helper.sh\n"
            "python3 scripts/ci/check_x.py\ngrep -q x scripts/ci/data.sh\n", encoding="utf-8")
        (scripts / "_helper.sh").write_text("bash scripts/ci/deep.sh\n", encoding="utf-8")
        (scripts / "data.sh").write_text("python3 scripts/ci/hidden.py\n", encoding="utf-8")
        (scripts / "test_x.py").write_text(
            "import subprocess\nsubprocess.run(['bash', 'scripts/ci/gate.sh'])\nSUBJECT = 'scripts/ci/data.sh'\n",
            encoding="utf-8")
        for name in ("commented.py", "check_x.py", "deep.sh", "hidden.py"):
            (scripts / name).write_text("\n", encoding="utf-8")
        assert script_closure("gate.sh", scripts) == ["_helper.sh", "check_x.py", "data.sh", "deep.sh"], "S1/S2"
        assert "gate.sh" in script_closure("test_x.py", scripts), "S3"
        assert "hidden.py" not in script_closure("test_x.py", scripts, mentions=True), "S2 data is not followed"
        assert "data.sh" in script_closure("test_x.py", scripts, mentions=True), "S4 mention"
        # Rule T: R1 a changed script selects the tests whose entry runs,
        # reads or names it (test_x.py names data.sh, runs gate.sh); R2 an
        # unrelated file selects nothing; R3 a test some pre-commit job already
        # runs for that path is reported as covered, not run twice; R4 an
        # orphan test_*.py that no gate names is still in the inventory.
        (scripts / "check-all.sh").write_text("run static x python3 scripts/ci/check_x.py --self-test\n",
                                              encoding="utf-8")
        (scripts / "check_x.py").write_text("import _shared\n", encoding="utf-8")
        (scripts / "_shared.py").write_text("\n", encoding="utf-8")
        script_edges.cache_clear()  # check_x.py was rewritten after its edges were read
        hooks = ("commit-checks:\n  jobs:\n    - name: x\n      glob: \"scripts/ci/_shared.py\"\n"
                 "      run: python3 scripts/ci/check_x.py --self-test\n")
        assert set(script_tests(scripts, hooks)) == {"python3 scripts/ci/check_x.py --self-test",
                                                     "python3 scripts/ci/test_x.py"}, "R4"
        picked = lambda path: {command: covered for command, _, covered
                               in selected_script_tests([path], scripts, hooks)}
        assert picked("scripts/ci/data.sh") == {"python3 scripts/ci/test_x.py": ""}, "R1 mention"
        assert picked("scripts/ci/deep.sh") == {"python3 scripts/ci/test_x.py": ""}, "R1 runs"
        assert picked("scripts/ci/hidden.py") == {}, "R2"
        assert picked("docs/a.md") == {}, "R2"
        # test_x.py runs gate.sh, which runs check_x.py, which imports _shared.py.
        assert picked("scripts/ci/_shared.py") == {"python3 scripts/ci/check_x.py --self-test": "x",
                                                   "python3 scripts/ci/test_x.py": ""}, "R3"
    print("generate_hooks self-test: generated globs follow their declarations")


# A script's tests run on the commit that changes it (rule T). The test
# inventory is what the gates already run: every scripts/ci self-test and
# test_*.py named in a gate script, plus any test_*.py nothing names (an orphan
# would otherwise never run). No second list is kept.
TEST_COMMAND = re.compile(
    r"python3 (?:-m unittest )?scripts/ci/(test_[A-Za-z0-9_]+\.py)\b"
    r"|python3 scripts/ci/([A-Za-z0-9_]+\.py) --self-?test\b(?: (?!-)[a-z][a-z-]*)?"
    r"|scripts/ci/([A-Za-z0-9_.-]+\.sh) --self-test\b")


def script_tests(scripts: Path = SCRIPTS, hooks: str | None = None) -> dict[str, str]:
    """``{command: entry script}`` for every test the gates run over ``scripts``."""
    found: dict[str, str] = {}
    sources = [path.read_text(encoding="utf-8") for path in sorted(scripts.glob("*.sh"))]
    sources.append(HOOKS.read_text(encoding="utf-8") if hooks is None else hooks)
    for source in sources:
        for line in source.splitlines():
            if line.lstrip().startswith("#"):
                continue
            for match in TEST_COMMAND.finditer(line):
                command = match.group(0).replace("--selftest", "--self-test").replace(" -m unittest", "")
                found.setdefault(command, next(group for group in match.groups() if group))
    imported = {module for test in scripts.glob("test_*.py") for module in local_imports(test.name, scripts)}
    for test in sorted(scripts.glob("test_*.py")):
        if test.name not in found.values() and test.name not in imported:
            found[f"python3 scripts/ci/{test.name}"] = test.name
    return found


def hook_triggers(hooks: str) -> dict[str, list[str]]:
    """Every pre-commit job's command body and rendered glob (``**/*`` when none)."""
    triggers = {}
    for name, block in command_blocks(hooks).items():
        glob = re.search(r'(?m)^\s+glob: "(.*)"$', block)
        triggers[name] = (_expand_braces(glob.group(1)) if glob else [UNIVERSAL], hook_run(block))
    return triggers


def selected_script_tests(paths: list[str], scripts: Path = SCRIPTS,
                          hooks: str | None = None) -> list[tuple[str, str, str]]:
    """``(command, reason, covered-by)`` for the tests a change to ``paths`` must run.

    A test is selected when a changed path is its entry script, anything that
    script runs or reads, or a file the entry itself names. A test some pre-commit job already runs for one
    of these paths is reported as covered by that job instead of run twice.
    """
    changed = {path.removeprefix("scripts/ci/") for path in paths if path.startswith("scripts/ci/")}
    if not changed:
        return []
    hooks = HOOKS.read_text(encoding="utf-8") if hooks is None else hooks
    jobs = hook_triggers(hooks)
    selected = []
    for command, entry in script_tests(scripts, hooks).items():
        # The entry, what it runs or reads, and what the entry itself names
        # (its subject); names inside its helpers are their data, not tests.
        subjects = {entry, *script_closure(entry, scripts), *script_edges(entry, scripts)[2]} & changed
        if not subjects:
            continue
        subject = sorted(subjects)[0]
        covering = sorted(job for job, (patterns, body) in jobs.items() if command in body
                          and any(glob_matches(pattern, f"scripts/ci/{name}") for pattern in patterns
                                  for name in subjects))
        selected.append((command, f"scripts/ci/{subject}", covering[0] if covering else ""))
    return selected


def run_script_tests(paths: list[str]) -> int:
    """Run every selected test not already run by another pre-commit job."""
    import subprocess
    import time
    failed = []
    for command, reason, covered in selected_script_tests(paths):
        if covered:
            print(f"↷ {command} (runs in pre-commit job {covered})", flush=True)
            continue
        started = time.monotonic()
        result = subprocess.run(shlex.split(command), cwd=ROOT, capture_output=True, text=True,
                                stdin=subprocess.DEVNULL)
        took = time.monotonic() - started
        if result.returncode == 0:
            print(f"✓ {command} ({took:.1f}s; tests {reason})", flush=True)
            continue
        failed.append(command)
        print(f"✗ {command} (exit {result.returncode}; tests {reason})\n"
              + "\n".join((result.stdout + result.stderr).splitlines()[-60:]))
    if failed:
        print("\nrule: a changed CI script's own tests run on the commit that changes it\n"
              "fix: make the failing tests above pass; rerun one with the printed command", file=sys.stderr)
        return 1
    return 0


def explain(paths: list[str]) -> list[str]:
    """Why each pre-commit job and script test runs (or not) for ``paths``."""
    lines = []
    hooks = HOOKS.read_text(encoding="utf-8")
    for job, (patterns, _) in hook_triggers(hooks).items():
        hits = [(path, pattern) for path in paths for pattern in patterns if glob_matches(pattern, path)]
        lines.append(f"{'runs ' if hits else 'skips'} {job}"
                     + (f": {hits[0][0]} matches {hits[0][1]}" if hits else ""))
    for command, reason, covered in selected_script_tests(paths, hooks=hooks):
        lines.append(f"test  {command}: tests {reason}" + (f" (inside {covered})" if covered else ""))
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail when lefthook.yml is stale, printing the diff",
    )
    parser.add_argument("--self-test", action="store_true", help="run the decision table")
    parser.add_argument("--script-tests", nargs="*", metavar="PATH",
                        help="run the tests of the changed scripts/ci files (pre-commit rule T)")
    parser.add_argument("--explain", nargs="+", metavar="PATH",
                        help="print which pre-commit jobs and script tests these paths start, and why")
    args = parser.parse_args(argv)

    if args.self_test:
        selftest()
        return 0
    if args.script_tests is not None:
        return run_script_tests(args.script_tests)
    if args.explain:
        print("\n".join(explain(args.explain)))
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
    execution_errors = hook_entry_errors(rendered) + generated_hook_glob_errors(rendered, declarations)
    if execution_errors:
        print('\n'.join(execution_errors), file=sys.stderr)
        return 1
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
