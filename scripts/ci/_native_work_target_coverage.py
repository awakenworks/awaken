"""Independent Cargo target coverage for the two native Work selectors."""
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
WORK_MANIFEST = Path("crates/platform/environment/awaken-work-store/Cargo.toml")


def validate_work_target_coverage(metadata, phases, root):
    packages = [p for p in metadata.get("packages", []) if p.get("name") == "awaken-work-store"]
    if len(packages) != 1:
        raise ValueError("Work target metadata must contain its one original package")
    package = packages[0]
    manifest = (Path(root) / WORK_MANIFEST).resolve()
    if Path(package.get("manifest_path", "")).resolve() != manifest:
        raise ValueError("Work target metadata selected a foreign manifest")
    original = set()
    libraries = 0
    for target in package.get("targets", []):
        kind, name = target.get("kind"), target.get("name")
        if target.get("test") is not True or not isinstance(name, str):
            raise ValueError("Work target metadata changed the current native-test policy")
        if not Path(target.get("src_path", "")).resolve().is_relative_to(manifest.parent):
            raise ValueError("Work target metadata selected a foreign source")
        if set(target.get("required-features", [])) - {"test-support"}:
            raise ValueError("Work target metadata added an unreviewed feature profile")
        if kind == ["lib"]:
            libraries += 1
            identity = ("lib",)
        elif kind == ["test"]:
            identity = ("test", name)
        else:
            raise ValueError("Work target metadata added an unreviewed target kind")
        if identity in original:
            raise ValueError("Work target metadata duplicated a target")
        original.add(identity)
    if libraries != 1:
        raise ValueError("Work target metadata must retain its original library")
    selected = []
    work = [p for p in phases if p.package == "awaken-work-store"]
    if len(work) != 2 or {p.selector for p in work} != {"backend_parity", "work_store_evidence"}:
        raise ValueError("Work targets must retain both independent native selectors")
    for phase in work:
        arguments = phase.arguments
        frontend = arguments[:arguments.index("--")] if "--" in arguments else arguments
        if any(arg in ("--tests", "--all-targets", "--bins", "--examples")
               or arg.startswith("--test=") for arg in frontend):
            raise ValueError("Work targets need explicit library/integration selection")
        selected += [("lib",)] * frontend.count("--lib")
        selected += [("test", frontend[index + 1]) for index, arg in enumerate(frontend) if arg == "--test"]
    if len(selected) != len(set(selected)) or set(selected) != original:
        missing, extra = original - set(selected), set(selected) - original
        raise ValueError("Work selectors differ from actual Cargo targets: missing=" + repr(sorted(missing))
                         + ", extra=" + repr(sorted(extra)))
    return {"package": package["name"], "manifest": str(manifest),
            "targets": [list(value) for value in sorted(original)]}


def read_work_metadata(root):
    command = [sys.executable, str(Path(root) / "scripts/ci/deadline.py"), "60", "--",
               "cargo", "metadata", "--locked", "--offline", "--no-deps", "--format-version", "1",
               "--manifest-path", str(WORK_MANIFEST), "--features", "test-support"]
    result = subprocess.run(command, cwd=root, capture_output=True)
    if result.returncode:
        sys.stderr.write(result.stderr.decode("utf-8", errors="replace"))
        raise ValueError("Work Cargo target metadata failed: " + str(result.returncode))
    try:
        return json.loads(result.stdout)
    except (ValueError, UnicodeError) as error:
        raise ValueError("Work Cargo target metadata is not valid JSON") from error


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    if argv != ["--check"]:
        raise SystemExit("usage: _native_work_target_coverage.py --check")
    from _native_postgres_scopes import postgres_phases
    report = validate_work_target_coverage(read_work_metadata(ROOT), postgres_phases("core", ROOT), ROOT)
    print(json.dumps(report))


if __name__ == "__main__":
    main()
