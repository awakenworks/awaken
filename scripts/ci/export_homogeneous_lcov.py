#!/usr/bin/env python3
"""Export one LCOV document per LLVM module-signature profile group.

Rust E2E processes do not necessarily contain identical function layouts. LLVM
therefore cannot safely merge every ``profraw`` file before export. Profile names
include ``%12m`` and this helper temporarily isolates each signature, asks
``cargo llvm-cov`` to export it, and restores every profile even on failure.
"""

from __future__ import annotations

import argparse
import collections
import os
import pathlib
import re
import shutil
import subprocess
import tempfile


ROOT = pathlib.Path(__file__).resolve().parents[2]
PROFILE = re.compile(r"^awaken-\d+-(\d+).*\.profraw$")


def profile_groups(directory: pathlib.Path) -> dict[str, list[pathlib.Path]]:
    groups: dict[str, list[pathlib.Path]] = collections.defaultdict(list)
    legacy: list[str] = []
    for path in sorted(directory.glob("awaken-*.profraw")):
        match = PROFILE.fullmatch(path.name)
        if match is None:
            legacy.append(path.name)
        else:
            groups[match.group(1)].append(path)
    if legacy:
        sample = ", ".join(legacy[:3])
        raise ValueError(
            "profile names lack an LLVM module signature; clean the coverage "
            f"target and use awaken-%p-%12m.profraw (examples: {sample})"
        )
    if not groups:
        raise ValueError(f"no signed awaken profiles found in {directory}")
    return dict(groups)


def export_groups(
    directory: pathlib.Path,
    output_directory: pathlib.Path,
    ignore_filename_regex: str | None,
) -> list[pathlib.Path]:
    groups = profile_groups(directory)
    output_directory.mkdir(parents=True, exist_ok=True)
    all_profiles = [path for paths in groups.values() for path in paths]
    reports: list[pathlib.Path] = []

    with tempfile.TemporaryDirectory(
        prefix="awaken-profraw-quarantine-", dir=directory.parent
    ) as temporary:
        quarantine = pathlib.Path(temporary)
        try:
            for signature, selected in sorted(groups.items()):
                selected_set = set(selected)
                moved = [path for path in all_profiles if path not in selected_set]
                for path in moved:
                    shutil.move(path, quarantine / path.name)
                report = (output_directory / f"awaken-{signature}.lcov").resolve()
                command = [
                    "cargo",
                    "llvm-cov",
                    "report",
                    "--lcov",
                    "--output-path",
                    str(report),
                ]
                if ignore_filename_regex:
                    command.extend(
                        ["--ignore-filename-regex", ignore_filename_regex]
                    )
                try:
                    environment = os.environ.copy()
                    environment["CARGO_LLVM_COV_TARGET_DIR"] = str(directory)
                    environment.setdefault("CARGO_TARGET_DIR", str(directory))
                    subprocess.run(
                        command, cwd=ROOT, check=True, env=environment
                    )
                    reports.append(report)
                finally:
                    for path in moved:
                        quarantined = quarantine / path.name
                        if quarantined.exists():
                            shutil.move(quarantined, path)
        finally:
            # A second recovery layer covers interruption between group exports.
            for quarantined in quarantine.glob("*.profraw"):
                destination = directory / quarantined.name
                if destination.exists():
                    raise RuntimeError(
                        f"cannot restore {quarantined.name}: destination exists"
                    )
                shutil.move(quarantined, destination)
    return reports


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--profile-directory", required=True, type=pathlib.Path)
    parser.add_argument("--output-directory", required=True, type=pathlib.Path)
    parser.add_argument("--ignore-filename-regex")
    args = parser.parse_args()
    try:
        reports = export_groups(
            args.profile_directory.resolve(),
            args.output_directory.resolve(),
            args.ignore_filename_regex,
        )
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    for report in reports:
        print(report)


if __name__ == "__main__":
    main()
