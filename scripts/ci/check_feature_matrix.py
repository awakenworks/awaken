#!/usr/bin/env python3
"""Keep every declared Cargo feature inside a gate that actually compiles it.

An optional feature is a second source tree.  Nothing but a compiler reads it,
so when the gate's feature set is smaller than the set of declared features,
the code behind the difference rots in silence: a trait method is renamed, a
port is deleted, a builder is added to one tier and not its sibling, and the
targets that would have caught it are never built.  Three live instances of
exactly that shape reached ``main`` together --- a deleted trait import in the
Docker/Podman host-staging oracle, a call to a port removed from ``Sandbox``
months earlier, and a package-build knob the Podman runtime never grew --- and
the first of them hid the other two, because a crate that fails to compile
stops its dependents from being compiled at all.

The cure is not a hand-maintained feature list; that is the same shape one
level up.  ``cargo`` already knows every feature of every workspace member, so
the gate compiles the *maximal* graph (``--workspace --all-features
--all-targets``) and this checker keeps that invocation from being narrowed and
keeps every declared feature inside its reach.  A feature that cargo cannot
reach that way --- a crate deliberately outside the workspace, say --- must
name the gate that verifies it instead.  Registering the substitute is the only
way past this check; there is no file allowlist to grow.

The isolated CLI dev-target row also keeps upstream test-support feature
unification from hiding a consumer-side cfg mismatch. Maximal features alone
cannot prove that row.

Compilation, not execution, is the obligation: the Docker, Podman and
Kubernetes tiers each need a live daemon to *run*, which their own integration
gates provide.  Their Cargo.toml comments already promise "Compile-verified";
this is the gate that keeps the promise.
"""

from __future__ import annotations

import argparse
import glob
import re
import shlex
import sys
import tomllib
from pathlib import Path

from generate_hooks import gate_mode_source

ROOT = Path(__file__).resolve().parents[2]
GATE = ROOT / "scripts/ci/check-rust.sh"

# The surface that reaches every feature of every workspace member. Written as
# the exact token sequence the gate must carry after the Cargo subcommand, so a
# narrowing edit (dropping --all-features, scoping to -p, excluding a package)
# fails here rather than silently shrinking the compile surface.
#
# The subcommand is deliberately not pinned to `check`. `cargo clippy` compiles
# exactly what `cargo check` compiles and additionally lints it, so a gate that
# replaced the two separate commands with one clippy over the same surface has
# not lost the compile obligation -- it has strengthened it. Pinning the word
# `check` would have refused that convergence and kept the lint permanently
# narrower than the compile.
MAXIMAL_COMPILE_SURFACE = "--workspace --all-features --all-targets"
MAXIMAL_COMPILE_SUBCOMMANDS = ("cargo check", "cargo clippy")
MAXIMAL_COMPILE = f"{MAXIMAL_COMPILE_SUBCOMMANDS[0]} {MAXIMAL_COMPILE_SURFACE}"


def compiles_maximal_graph(text: str) -> bool:
    """Does this text run some Cargo command over the maximal feature graph?"""

    return any(
        f"{subcommand} {MAXIMAL_COMPILE_SURFACE}" in text
        for subcommand in MAXIMAL_COMPILE_SUBCOMMANDS
    )

# Modes whose body must carry the maximal compile. --quick is deliberately
# absent: it is the per-commit gate and stays fast; the maximal graph is a
# pre-push and full-run obligation, the same altitude as workspace clippy.
MAXIMAL_COMPILE_MODES = ("--push", "--full")

# "<crate>/<feature>" -> the gate that compiles it instead of the maximal
# workspace graph. Only a feature cargo cannot reach from the workspace root
# belongs here, and the entry must say who does reach it. An entry whose crate
# or feature no longer exists, or that the maximal graph now covers, is stale
# and refused: this registry cannot quietly outlive its reason.
FEATURE_VERIFICATION_EXEMPTIONS: dict[str, str] = {}

SKIP_DIRECTORIES = ("target", "node_modules", ".git")


def crate_features(manifest: Path) -> tuple[str, tuple[str, ...]]:
    """One manifest's package name and its non-default feature names."""

    document = tomllib.loads(manifest.read_text(encoding="utf-8"))
    name = document.get("package", {}).get("name", manifest.parent.name)
    features = document.get("features", {})
    declared = tuple(
        sorted(feature for feature in features if feature != "default")
    ) if isinstance(features, dict) else ()
    return name, declared


def workspace_manifests(root: Path) -> set[Path]:
    """Every member manifest the workspace root resolves, as absolute paths."""

    document = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    workspace = document.get("workspace", {})
    manifests: set[Path] = set()
    for pattern in workspace.get("members", []):
        for match in glob.glob(str(root / pattern / "Cargo.toml")):
            manifests.add(Path(match).resolve())
    return manifests


def repository_manifests(root: Path) -> list[Path]:
    """Every crate manifest in the tree except the workspace root itself."""

    found: list[Path] = []
    for manifest in root.rglob("Cargo.toml"):
        if any(part in SKIP_DIRECTORIES for part in manifest.relative_to(root).parts):
            continue
        if manifest.resolve() == (root / "Cargo.toml").resolve():
            continue
        found.append(manifest)
    return sorted(found)


def maximal_compile_errors(gate: str) -> list[str]:
    """Cause/effect for the compile surface the gate must keep.

    | Rule | maximal invocation present | in every required mode | Effect |
    |------|----------------------------|------------------------|--------|
    | M1   | yes                        | yes                    | accept |
    | M2   | no                         | --                     | reject |
    | M3   | yes                        | no                     | reject |

    A mode body is read as the text between its ``case`` label and the ``;;``
    that closes it, so a command sitting in a neighbouring branch cannot be
    mistaken for one this mode runs.
    """

    errors: list[str] = []
    if not compiles_maximal_graph(gate):
        errors.append(
            f"scripts/ci/check-rust.sh no longer runs any of "
            f"{' / '.join(f'`{s} {MAXIMAL_COMPILE_SURFACE}`' for s in MAXIMAL_COMPILE_SUBCOMMANDS)}"
            "; without one a declared feature can stop compiling and no gate reports it"
        )
        return errors
    for mode in MAXIMAL_COMPILE_MODES:
        body = mode_body(gate, mode)
        if body is None:
            errors.append(f"scripts/ci/check-rust.sh has no {mode} branch to compile features in")
        elif not compiles_maximal_graph(body):
            errors.append(
                f"scripts/ci/check-rust.sh {mode} does not run `{MAXIMAL_COMPILE}`, so the "
                f"features outside its own feature set are not compiled by {mode}"
            )
    return errors


MINIMAL_CLI_DEV_COMPILE = "cargo check -p awaken-cli --no-default-features --example credential_reference_worker"


def compiles_minimal_cli_dev(text: str) -> bool:
    expected = shlex.split(MINIMAL_CLI_DEV_COMPILE)
    for line in re.sub(r"\\\r?\n", " ", text).splitlines():
        try:
            words = shlex.split(line, comments=True)
        except ValueError:
            continue
        if not words or words[0] not in ("cargo", "step", "scoped_step") or "cargo" not in words:
            continue
        command = words[words.index("cargo"):]
        if (command[:len(expected)] == expected
                and not any(word in ("--all-features", "--features", "-F") or word.startswith("--features=") for word in command)
                and ("--locked" in command or "${locked[@]}" in command)):
            return True
    return False


def dev_target_compile_errors(gate: str) -> list[str]:
    errors = []
    for mode in ("--push", "--full"):
        if not compiles_minimal_cli_dev(gate_mode_source(gate, mode)):
            errors.append(f"{mode} must compile the isolated CLI dev target without enabling its own test-support")
    if compiles_minimal_cli_dev(gate_mode_source(gate, "--quick")):
        errors.append("the CLI dev feature-union regression belongs to push/full, not quick")
    return errors


def mode_body(gate: str, mode: str) -> str | None:
    """The text of one ``case`` branch, or None when the branch is absent."""

    pattern = re.compile(
        r"(?ms)^  (?:[^\n)]*\|)?" + re.escape(mode) + r"(?:\|[^\n)]*)?\)\n(.*?)^    ;;"
    )
    match = pattern.search(gate)
    return match.group(1) if match else None


def unreachable_feature_errors(
    manifests: list[Path],
    members: set[Path],
    exemptions: dict[str, str],
    root: Path,
) -> list[str]:
    """Cause/effect for reachability of one crate's declared features.

    | Rule | declares features | is a workspace member | registered | Effect |
    |------|-------------------|-----------------------|------------|--------|
    | R1   | yes               | yes                   | no         | accept |
    | R2   | yes               | no                    | no         | reject |
    | R3   | yes               | no                    | yes        | accept |
    | R4   | yes               | yes                   | yes        | reject (stale) |
    | R5   | no                | no                    | no         | accept |

    R4 is the rule that keeps the registry honest: once the maximal graph
    reaches a feature, the entry claiming someone else verifies it is a lie
    about where the evidence comes from, so it must be deleted.
    """

    errors: list[str] = []
    live: set[str] = set()
    for manifest in manifests:
        name, declared = crate_features(manifest)
        member = manifest.resolve() in members
        for feature in declared:
            key = f"{name}/{feature}"
            registered = key in exemptions
            if member and registered:
                errors.append(
                    f"FEATURE_VERIFICATION_EXEMPTIONS names {key}, but the maximal workspace "
                    "graph already compiles it; drop the stale exemption"
                )
                live.add(key)
            elif member:
                continue
            elif registered:
                live.add(key)
            else:
                relative = manifest.relative_to(root)
                errors.append(
                    f"{relative} declares feature '{feature}' but the crate is not a workspace "
                    "member, so no gate compiles it; add the crate to [workspace.members] or "
                    f"register {key} in FEATURE_VERIFICATION_EXEMPTIONS with the gate that does"
                )
    for key, verifier in sorted(exemptions.items()):
        if key not in live:
            errors.append(
                f"FEATURE_VERIFICATION_EXEMPTIONS names {key} (verified by {verifier}), "
                "which no longer exists; drop the stale exemption"
            )
    return errors


def compile_surface(manifests: list[Path], members: set[Path]) -> tuple[int, int]:
    """Declared features reached by the maximal graph, and the crates holding them."""

    features = 0
    crates = 0
    for manifest in manifests:
        if manifest.resolve() not in members:
            continue
        _, declared = crate_features(manifest)
        if declared:
            crates += 1
            features += len(declared)
    return features, crates


def selftest() -> None:
    """Prove both decision tables on synthetic inputs before judging the repo."""

    import tempfile

    gate = (
        "case \"$mode\" in\n"
        "  --quick|--push)\n"
        "    step check true\n"
        "    if [ \"$mode\" = \"--push\" ]; then\n"
        f"      step all-features {MAXIMAL_COMPILE} \"${{locked[@]}}\"\n"
        "    fi\n"
        "    ;;\n"
        "  --full)\n"
        f"    step all-features {MAXIMAL_COMPILE} \"${{locked[@]}}\"\n"
        "    ;;\n"
        "esac\n"
    )
    assert not maximal_compile_errors(gate), "M1"
    assert maximal_compile_errors(
        gate.replace(MAXIMAL_COMPILE, "cargo check --workspace --all-targets")
    ), "M2 a narrowed invocation is not the maximal graph"
    dropped_from_full = gate[: gate.index("  --full)")] + "  --full)\n    step clippy true\n    ;;\nesac\n"
    assert maximal_compile_errors(dropped_from_full), "M3 --full must compile it too"
    # A command in a neighbouring branch does not satisfy the branch that lost it.
    assert maximal_compile_errors(
        gate.replace("  --full)\n", "  --other)\n", 1)
    ), "M4 branch bodies are read separately"
    # M5 the obligation is a compile surface, not a subcommand: `cargo clippy`
    # over the same surface compiles everything `cargo check` does and lints it
    # too, so replacing the two commands with one is admitted. M6 a clippy over
    # a *narrower* surface is still refused.
    assert not maximal_compile_errors(
        gate.replace(MAXIMAL_COMPILE, f"cargo clippy {MAXIMAL_COMPILE_SURFACE}")
    ), "M5 clippy over the maximal surface compiles the maximal graph"
    assert maximal_compile_errors(
        gate.replace(MAXIMAL_COMPILE, "cargo clippy --workspace --all-targets")
    ), "M6 a narrower clippy is not the maximal graph"

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        (root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crates/*"]\n', encoding="utf-8"
        )
        for name, features, where in (
            ("member-with-features", ["docker", "podman"], "crates/member-with-features"),
            ("member-plain", [], "crates/member-plain"),
            ("outsider", ["fuzzing"], "fuzz"),
        ):
            manifest = root / where / "Cargo.toml"
            manifest.parent.mkdir(parents=True, exist_ok=True)
            rendered = "".join(f'{feature} = []\n' for feature in features)
            manifest.write_text(
                f'[package]\nname = "{name}"\n\n[features]\n{rendered}', encoding="utf-8"
            )
        manifests = repository_manifests(root)
        members = workspace_manifests(root)
        errors = unreachable_feature_errors(manifests, members, {}, root)
        assert len(errors) == 1 and "outsider" in errors[0], f"R2 {errors}"
        assert not unreachable_feature_errors(
            manifests, members, {"outsider/fuzzing": "the cargo-fuzz gate"}, root
        ), "R1 + R3 + R5"
        stale = unreachable_feature_errors(
            manifests,
            members,
            {"outsider/fuzzing": "the cargo-fuzz gate", "member-with-features/docker": "nobody"},
            root,
        )
        assert len(stale) == 1 and "already compiles it" in stale[0], f"R4 {stale}"
        gone = unreachable_feature_errors(
            manifests, members, {"outsider/fuzzing": "x", "deleted/feature": "y"}, root
        )
        assert len(gone) == 1 and "no longer exists" in gone[0], f"R4 removed crate {gone}"
        assert compile_surface(manifests, members) == (2, 1), "the reported surface counts members"
    # U1 actual minimal dev target in push/full; U2 omitted, comment, print,
    # or explicit CLI test-support masks the cross-crate feature-union case.
    command = MINIMAL_CLI_DEV_COMPILE + ' "${locked[@]}"'
    prefix = 'mode="${1:---full}"\ncase "$mode" in\n  --quick|--push|--full)\n    ;;\nesac\n'
    isolated = prefix + ('case "$mode" in\n  --quick|--push)\n'
        '    if [ "$mode" = "--push" ]; then\n      ' + command + '\n    fi\n    ;;\n'
        '  --full)\n    ' + command + '\n    ;;\nesac\n')
    assert not dev_target_compile_errors(isolated), "U1"
    for replacement in ("true", "# " + command, "echo " + command,
                        command + " --all-features", command + " --features test-support",
                        command.replace("--no-default-features", "")):
        assert dev_target_compile_errors(isolated.replace(command, replacement)), ("U2", replacement)
    assert dev_target_compile_errors(isolated.replace('if [ "$mode" = "--push" ]',
                                                       'if [ "$mode" = "--quick" ]')), "U3 quick cannot substitute for push"
    print("check_feature_matrix self-test: maximal, reachability and isolated dev tables hold")


def main(arguments: argparse.Namespace) -> int:
    if arguments.self_test:
        selftest()
        return 0
    selftest()

    gate = GATE.read_text(encoding="utf-8")
    manifests = repository_manifests(ROOT)
    members = workspace_manifests(ROOT)
    errors = maximal_compile_errors(gate)
    errors += dev_target_compile_errors(gate)
    errors += unreachable_feature_errors(
        manifests, members, FEATURE_VERIFICATION_EXEMPTIONS, ROOT
    )
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    features, crates = compile_surface(manifests, members)
    registered = len(FEATURE_VERIFICATION_EXEMPTIONS)
    print(
        f"OK - {features} optional features across {crates} crates are compiled by the "
        f"maximal workspace graph ({registered} verified elsewhere)."
    )
    return 0


# One canonical spelling per flag, and nothing else admitted. A checker that
# accepted an unknown argument and fell through to its normal scan exited zero
# having run something other than what the caller asked for: that is how
# `check-all.sh` invoked `--selftest` for years while the flag was `--self-test`
# and the self-test never once executed. ``allow_abbrev=False`` closes the
# second half: without it ``--self-tes`` is also accepted, so the flag has no
# single spelling.
def parse(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="check_feature_matrix.py", description=__doc__, allow_abbrev=False
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the decision table instead of the scan"
    )
    return parser.parse_args(argv)


if __name__ == "__main__":
    raise SystemExit(main(parse(sys.argv[1:])))
