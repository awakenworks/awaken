#!/usr/bin/env python3
"""Refuse an SDK public-API change that its crate version does not announce.

The published SDK is every crate with `product = "sdk"` in
`[package.metadata.awaken]` that Cargo may publish (the same selection
`public_api_selection.py` makes for the snapshot gate). Its SemVer reference is
the last SDK release, named once as `sdk_semver_baseline` in the workspace
`[workspace.metadata.awaken]`: a commit whose `public-api/<crate>.txt` and crate
versions are what downstream code was written against.

For each SDK crate the committed snapshot is compared with the baseline's
(`check_public_api.sh` proves the committed snapshot is the real surface), and
the difference sets the bump the crate version must carry relative to the
baseline version. Versions are compared the way Cargo resolves them: `X.y.z`
for X >= 1, `0.Y.z`, and `0.0.Z` are the compatibility-breaking positions.
A crate whose version moved also needs a `## [<version>]` section in its
`CHANGELOG.md`. A crate that has no baseline snapshot is new: its first release
carries no obligation.

The snapshot is read conservatively. Rustdoc text cannot see private fields,
provided-vs-required trait methods or auto traits, so an added enum variant,
struct field or trait item counts as breaking unless the type is
`#[non_exhaustive]`; the release qualifier (`check_sdk_consumer.py
--qualify-semver`, cargo-semver-checks) remains the authority on what the text
cannot show. Policy: docs/design/sdk-versioning.md.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

INPUT_GLOBS: tuple[str, ...] = (
    "Cargo.toml",
    "crates/**/Cargo.toml",
    "crates/**/CHANGELOG.md",
    "public-api/*.txt",
)

import _crate_boundary_workspace as workspace

ROOT = Path(__file__).resolve().parents[2]

LEVELS = ("none", "patch", "additive", "breaking")


@dataclass(frozen=True)
class SdkCrate:
    name: str
    directory: str
    version: str


@dataclass(frozen=True)
class Verdict:
    crate: str
    baseline_version: str | None
    version: str
    required: str
    actual: str
    problems: tuple[str, ...]


# --------------------------------------------------------------------------
# Versions


def parse_version(text: str) -> tuple[int, int, int]:
    match = re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", text)
    if match is None:
        raise ValueError(f"version {text!r} is not a plain MAJOR.MINOR.PATCH release")
    return int(match[1]), int(match[2]), int(match[3])


def bump_level(old: str, new: str) -> str:
    """The SemVer level of `old -> new`, in Cargo's compatibility positions."""
    before, after = parse_version(old), parse_version(new)
    if after < before:
        raise ValueError(f"version went backwards: {old} -> {new}")
    if after == before:
        return "none"

    def compatible(version: tuple[int, int, int]) -> tuple[int, ...]:
        major, minor, patch = version
        return (major,) if major else (0, minor) if minor else (0, 0, patch)

    if compatible(before) != compatible(after):
        return "breaking"
    if before[0] == 0:
        return "additive"  # 0.Y.z: z is the minor-like position
    return "additive" if before[1] != after[1] else "patch"


# --------------------------------------------------------------------------
# Snapshot classification

_DECLARATION = re.compile(
    r"(?P<attrs>(?:#\[[^\]]*\] )*)pub (?:unsafe )?(?P<kind>enum|struct|trait|union) (?P<path>[A-Za-z0-9_:]+)"
)
_ITEM = re.compile(r"pub (?:(?:const |async |unsafe |extern \"[^\"]*\" )*fn |type |const |static )(?P<path>[A-Za-z0-9_:]+)")
_MEMBER = re.compile(r"pub (?P<path>[A-Za-z0-9_:]+)(?P<rest>[(:].*)?$")
_DROP = re.compile(r"impl(?:<[^>]*>)? core::ops::drop::Drop for (?P<path>[A-Za-z0-9_:]+)")


def _declarations(lines: set[str]) -> dict[str, tuple[str, bool]]:
    found = {}
    for line in lines:
        match = _DECLARATION.match(line)
        if match:
            found[match["path"]] = (match["kind"], "#[non_exhaustive]" in match["attrs"])
    return found


def _has_public_fields(path: str, lines: set[str]) -> bool:
    prefix = path + "::"
    return any(
        (member := _MEMBER.fullmatch(line)) is not None
        and member["path"].startswith(prefix)
        and (member["rest"] or "").startswith(":")
        for line in lines
    )


def _breaking_addition(line: str, old: set[str], new: set[str],
                       before: dict[str, tuple[str, bool]], after: dict[str, tuple[str, bool]]) -> str | None:
    """Why an added snapshot line breaks downstream code, or None if it is additive."""
    if (drop := _DROP.match(line)) and drop["path"] in before and _has_public_fields(drop["path"], new):
        return f"Drop added to {drop['path']}, which has public fields (destructuring moves out)"
    if (item := _ITEM.match(line)) is not None:
        owner = item["path"].rpartition("::")[0]
        if before.get(owner, ("", False))[0] == "trait":
            return f"item added to existing trait {owner}"
        return None
    member = _MEMBER.fullmatch(line)
    if member is None or line.startswith(("pub mod ", "pub use ", "pub macro ")) or _DECLARATION.match(line):
        return None
    owner, _, _ = member["path"].rpartition("::")
    kind, non_exhaustive = before.get(owner, ("", False))
    if kind == "enum" and not after.get(owner, ("", True))[1]:
        return f"variant added to exhaustive enum {owner}"
    if kind == "struct" and not after.get(owner, ("", True))[1]:
        return f"public field added to exhaustive struct {owner}"
    enum, _, _ = owner.rpartition("::")
    if before.get(enum, ("", False))[0] == "enum" and f"pub {owner}" in {
        line.split("(")[0] for line in old
    }:
        return f"field added to existing variant {owner}"
    return None


def classify_snapshot(old_text: str, new_text: str) -> tuple[str, list[str]]:
    """The bump a snapshot change requires, with the lines that decided it."""
    old = {line for line in old_text.splitlines() if line.strip()}
    new = {line for line in new_text.splitlines() if line.strip()}
    removed, added = sorted(old - new), sorted(new - old)
    if removed:
        return "breaking", [f"removed or changed: {line}" for line in removed[:5]]
    before, after = _declarations(old), _declarations(new)
    reasons = [reason for line in added if (reason := _breaking_addition(line, old, new, before, after))]
    if reasons:
        return "breaking", reasons[:5]
    return ("additive", [f"added: {line}" for line in added[:3]]) if added else ("none", [])


# --------------------------------------------------------------------------
# Repository reads


def _git(*arguments: str) -> subprocess.CompletedProcess[str]:
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    return subprocess.run(["git", *arguments], cwd=ROOT, env=environment, text=True,
                          capture_output=True, check=False)


def _show(revision: str, path: str) -> str | None:
    result = _git("show", f"{revision}:{path}")
    return result.stdout if result.returncode == 0 else None


def _resolved_version(manifest: dict, workspace_manifest: dict) -> str:
    version = manifest["package"].get("version", "0.0.0")
    if isinstance(version, dict):
        if version != {"workspace": True}:
            raise ValueError("a package version inherits only `version.workspace = true`")
        version = workspace_manifest["workspace"]["package"]["version"]
    return str(version)


def _published_sdk(manifest: dict) -> bool:
    package = manifest.get("package", {})
    metadata = (package.get("metadata") or {}).get("awaken") or {}
    return metadata.get("product") == "sdk" and package.get("publish", True) not in (False, [])


def current_sdk() -> tuple[str, list[SdkCrate]]:
    root_manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    baseline = ((root_manifest.get("workspace", {}).get("metadata") or {}).get("awaken") or {}).get(
        "sdk_semver_baseline")
    if not isinstance(baseline, str) or not re.fullmatch(r"[0-9a-f]{40}", baseline):
        raise ValueError("Cargo.toml [workspace.metadata.awaken] sdk_semver_baseline must be a full commit id")
    crates = []
    for path in workspace.crate_manifests(ROOT / "crates"):
        manifest = workspace.load_manifest(path)
        if _published_sdk(manifest):
            crates.append(SdkCrate(workspace.package_name(manifest),
                                   path.parent.relative_to(ROOT).as_posix(),
                                   _resolved_version(manifest, root_manifest)))
    return baseline, sorted(crates, key=lambda crate: crate.name)


def baseline_version(revision: str, name: str) -> str:
    listing = _git("ls-tree", "-r", "--name-only", revision, "--", "crates")
    if listing.returncode != 0:
        raise ValueError(f"cannot list the SDK baseline {revision[:10]}; fetch it ({listing.stderr.strip()})")
    root = _show(revision, "Cargo.toml")
    for path in listing.stdout.splitlines():
        if path.endswith(f"/{name}/Cargo.toml") and root is not None:
            text = _show(revision, path)
            manifest = tomllib.loads(text) if text else {}
            if manifest.get("package", {}).get("name") == name:
                return _resolved_version(manifest, tomllib.loads(root))
    raise ValueError(f"{name}: the baseline has its snapshot but no manifest")


def changelog_has(directory: str, version: str) -> bool:
    path = ROOT / directory / "CHANGELOG.md"
    if not path.is_file():
        return False
    heading = re.compile(rf"^## \[{re.escape(version)}\]", re.MULTILINE)
    return heading.search(path.read_text(encoding="utf-8")) is not None


# --------------------------------------------------------------------------
# Decision


def decide(crate: SdkCrate, old_snapshot: str | None, new_snapshot: str,
           old_version: str | None, has_changelog: bool,
           breaking_dependencies: frozenset[str] = frozenset()) -> Verdict:
    """Decision table D1-D9 (see `self_test`).

    `breaking_dependencies` are the other SDK crates whose version is a
    breaking bump; a crate whose public API names one of their items exposes a
    changed type identity and is breaking itself (D9).
    """
    if old_snapshot is None or old_version is None:
        return Verdict(crate.name, None, crate.version, "none", "new", ())  # D6
    required, evidence = classify_snapshot(old_snapshot, new_snapshot)
    exposed = sorted(name for name in breaking_dependencies
                     if name != crate.name and name.replace("-", "_") + "::" in new_snapshot)
    if exposed and required != "breaking":
        required = "breaking"
        evidence = [f"public API names items of {name}, whose version is a breaking bump" for name in exposed]
    problems: list[str] = []
    try:
        actual = bump_level(old_version, crate.version)
    except ValueError as error:
        return Verdict(crate.name, old_version, crate.version, required, "invalid", (str(error),))
    if LEVELS.index(actual) < LEVELS.index(required):
        target = "a major (0.x: minor)" if required == "breaking" else "a minor (0.x: patch)"
        problems.append(f"public API change needs {target} bump over {old_version}, found {crate.version}")
        problems.extend("  " + line for line in evidence)
    if actual != "none" and not has_changelog:
        problems.append(f"{crate.directory}/CHANGELOG.md has no `## [{crate.version}]` section")
    return Verdict(crate.name, old_version, crate.version, required, actual, tuple(problems))


def check(report: bool) -> int:
    try:
        revision, crates = current_sdk()
    except (ValueError, KeyError, tomllib.TOMLDecodeError) as error:
        print(f"SDK SemVer: {error}", file=sys.stderr)
        return 1
    if _git("cat-file", "-e", f"{revision}^{{commit}}").returncode != 0:
        print(f"SDK SemVer: baseline {revision} is not in this clone; fetch full history", file=sys.stderr)
        return 1
    inputs = []
    for crate in crates:
        snapshot = ROOT / "public-api" / f"{crate.name}.txt"
        if not snapshot.is_file():
            print(f"SDK SemVer: {crate.name} has no public-api snapshot", file=sys.stderr)
            return 1
        old_snapshot = _show(revision, f"public-api/{crate.name}.txt")
        try:
            old_version = baseline_version(revision, crate.name) if old_snapshot is not None else None
            moved = old_version is not None and bump_level(old_version, crate.version) == "breaking"
        except ValueError as error:
            print(f"SDK SemVer: {crate.name}: {error}", file=sys.stderr)
            return 1
        inputs.append((crate, old_snapshot, snapshot.read_text(encoding="utf-8"), old_version, moved))
    breaking = frozenset(crate.name for crate, *_, moved in inputs if moved)
    verdicts = [decide(crate, old_snapshot, new_snapshot, old_version,
                       changelog_has(crate.directory, crate.version), breaking)
                for crate, old_snapshot, new_snapshot, old_version, _ in inputs]
    failed = [verdict for verdict in verdicts if verdict.problems]
    for verdict in verdicts if report else failed:
        print(f"{verdict.crate}: {verdict.baseline_version or '-'} -> {verdict.version}"
              f"  surface={verdict.required} version={verdict.actual}")
        for problem in verdict.problems:
            print(f"    {problem}")
    if failed:
        print(f"❌ SDK SemVer: {len(failed)} of {len(verdicts)} SDK crates do not announce their "
              f"public API change against {revision[:10]} (docs/design/sdk-versioning.md)", file=sys.stderr)
        return 1
    print(f"✓ SDK SemVer: {len(verdicts)} SDK crates versioned against baseline {revision[:10]}")
    return 0


# --------------------------------------------------------------------------
# Self-test


def self_test() -> None:
    # | Rule | Versions          | Level     |
    # | V1   | 1.0.0 -> 1.0.0    | none      |
    # | V2   | 1.0.0 -> 1.0.1    | patch     |
    # | V3   | 1.0.0 -> 1.1.0    | additive  |
    # | V4   | 1.0.0 -> 2.0.0    | breaking  |
    # | V5   | 0.1.0 -> 0.1.1    | additive  |
    # | V6   | 0.1.0 -> 0.2.0    | breaking  |
    # | V7   | 0.0.1 -> 0.0.2    | breaking  |
    # | V8   | 2.0.0 -> 1.0.0    | refused   |
    # | V9   | 2.0.0-rc.1        | refused   |
    for old, new, level in (("1.0.0", "1.0.0", "none"), ("1.0.0", "1.0.1", "patch"),
                            ("1.0.0", "1.1.0", "additive"), ("1.0.0", "2.0.0", "breaking"),
                            ("0.1.0", "0.1.1", "additive"), ("0.1.0", "0.2.0", "breaking"),
                            ("0.0.1", "0.0.2", "breaking")):
        assert bump_level(old, new) == level, (old, new)
    for old, new in (("2.0.0", "1.0.0"), ("1.0.0", "2.0.0-rc.1")):
        try:
            bump_level(old, new)
        except ValueError:
            continue
        raise AssertionError(f"V8/V9 {old} -> {new}")

    base = "\n".join((
        "pub mod c",
        "pub enum c::E",
        "pub c::E::A",
        "pub c::E::S",
        "pub c::E::S::x: u8",
        "#[non_exhaustive] pub enum c::N",
        "pub c::N::A",
        "pub struct c::P",
        "pub c::P::x: u8",
        "#[non_exhaustive] pub struct c::Q",
        "pub c::Q::x: u8",
        "pub struct c::H",
        "pub trait c::T",
        "pub fn c::T::m(&self)",
        "pub fn c::f()",
    ))

    def level(*added: str, removed: str = "") -> str:
        old = base.splitlines()
        new = [line for line in old if line != removed] + list(added)
        return classify_snapshot(base, "\n".join(new))[0]

    # | Rule | Snapshot change                                   | Required |
    # | S1   | identical                                         | none     |
    # | S2   | a line removed                                    | breaking |
    # | S3   | a signature changed (removed + added)             | breaking |
    # | S4   | new function, type or module                      | additive |
    # | S5   | variant added to an existing exhaustive enum      | breaking |
    # | S6   | variant added to a #[non_exhaustive] enum         | additive |
    # | S7   | a new enum with its variants                      | additive |
    # | S8   | field added to exhaustive / non_exhaustive struct | breaking / additive |
    # | S9   | item added to an existing trait                   | breaking |
    # | S10  | inherent method added to an existing struct       | additive |
    # | S11  | field added to an existing enum variant           | breaking |
    # | S12  | Drop added: type with / without public fields     | breaking / additive |
    assert level() == "none", "S1"
    assert level(removed="pub fn c::f()") == "breaking", "S2"
    assert level("pub fn c::f(u8)", removed="pub fn c::f()") == "breaking", "S3"
    assert level("pub fn c::g()", "pub struct c::R", "pub mod c::m") == "additive", "S4"
    assert level("pub c::E::B") == "breaking", "S5"
    assert level("pub c::E::B(u8)") == "breaking", "S5 tuple"
    assert level("pub c::N::B") == "additive", "S6"
    assert level("pub enum c::F", "pub c::F::A", "pub c::F::A::y: u8") == "additive", "S7"
    assert level("pub c::P::y: u8") == "breaking", "S8"
    assert level("pub c::Q::y: u8") == "additive", "S8 non_exhaustive"
    assert level("pub fn c::T::n(&self)") == "breaking", "S9"
    assert level("pub type c::T::Out") == "breaking", "S9 associated type"
    assert level("impl c::P", "pub fn c::P::new() -> Self") == "additive", "S10"
    assert level("pub c::E::S::y: u8") == "breaking", "S11"
    assert level("impl core::ops::drop::Drop for c::P") == "breaking", "S12"
    assert level("impl core::ops::drop::Drop for c::H") == "additive", "S12 no fields"

    crate = SdkCrate("c", "crates/x/c", "1.0.0")
    old = "pub fn c::f()"

    def verdict(new: str, version: str, changelog: bool = True, previous: str | None = old,
                old_version: str | None = "1.0.0") -> Verdict:
        return decide(SdkCrate("c", "crates/x/c", version), previous, new, old_version, changelog)

    # | Rule | Required | Version bump        | Changelog | Result |
    # | D1   | none     | none / any          | -         | pass   |
    # | D2   | additive | none / patch        | -         | refuse |
    # | D3   | additive | minor / major       | present   | pass   |
    # | D4   | breaking | none / patch / minor| -         | refuse |
    # | D5   | breaking | major               | present   | pass   |
    # | D6   | new crate (no baseline snapshot)| -         | pass   |
    # | D7   | any      | moved               | missing   | refuse |
    # | D8   | any      | version went back   | -         | refuse |
    # | D9   | surface unchanged, names items of a crate with a breaking bump | major | pass; else refuse |
    assert not verdict(old, "1.0.0").problems and not verdict(old, "1.0.1").problems, "D1"
    added = old + "\npub fn c::g()"
    assert verdict(added, "1.0.0").problems and verdict(added, "1.0.1").problems, "D2"
    assert not verdict(added, "1.1.0").problems and not verdict(added, "2.0.0").problems, "D3"
    for version in ("1.0.0", "1.0.1", "1.1.0"):
        assert verdict("pub fn c::f(u8)", version).problems, f"D4 {version}"
    assert not verdict("pub fn c::f(u8)", "2.0.0").problems, "D5"
    assert not verdict("pub fn c::f(u8)", "0.2.0", old_version="0.1.0").problems, "D5 0.x"
    assert not decide(crate, None, "pub fn c::anything()", None, False).problems, "D6"
    assert verdict(added, "1.1.0", changelog=False).problems, "D7"
    assert not verdict(old, "1.0.0", changelog=False).problems, "D7 unmoved"
    assert verdict(old, "0.9.0").actual == "invalid" and verdict(old, "0.9.0").problems, "D8"
    exposing = "pub fn c::f(dep::T)"
    dependency = frozenset({"dep", "c"})
    assert decide(SdkCrate("c", "d", "1.0.0"), exposing, exposing, "1.0.0", True, dependency).problems, "D9"
    assert not decide(SdkCrate("c", "d", "2.0.0"), exposing, exposing, "1.0.0", True, dependency).problems, "D9 major"
    assert not decide(SdkCrate("c", "d", "1.0.0"), old, old, "1.0.0", True, dependency).problems, "D9 not named"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0], allow_abbrev=False)
    parser.add_argument("--self-test", action="store_true", help="run the decision tables and exit")
    parser.add_argument("--report", action="store_true", help="print every SDK crate, not only failures")
    arguments = parser.parse_args()
    self_test()
    if arguments.self_test:
        print("✓ check_sdk_semver self-test")
        return 0
    return check(arguments.report)


if __name__ == "__main__":
    raise SystemExit(main())
