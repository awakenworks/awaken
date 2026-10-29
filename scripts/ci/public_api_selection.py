"""The existing public-API metadata selection, shared by SDK qualification."""
from __future__ import annotations

import json
import re
import sys


def selected_crates(packages: list[dict]) -> tuple[list[str], list[str]]:
    snapshot_targets(packages)
    sdk = [p for p in packages
           if ((p.get("metadata") or {}).get("awaken") or {}).get("product") == "sdk"
           and p.get("publish") != []]
    missing_sdk = sorted(p["name"] for p in sdk
                         if ((p.get("metadata") or {}).get("awaken") or {}).get("api_snapshot") is not True)
    if missing_sdk:
        raise ValueError("publishable SDK crates must explicitly select public API snapshots: "
                         + ", ".join(missing_sdk))
    selected = sorted(p["name"] for p in packages
                      if ((p.get("metadata") or {}).get("awaken") or {}).get("api_snapshot") is True)
    return selected, sorted(p["name"] for p in sdk)


def snapshot_targets(packages: list[dict]) -> dict[str, str]:
    """A platform API is rendered for its authored target on every host."""
    targets = {}
    for package in packages:
        metadata = ((package.get("metadata") or {}).get("awaken") or {})
        if "api_snapshot_target" not in metadata:
            continue
        target = metadata["api_snapshot_target"]
        if (metadata.get("api_snapshot") is not True or not isinstance(target, str)
                or not re.fullmatch(r"[a-z0-9]+(?:[-_][a-z0-9]+)+", target)):
            raise ValueError(package["name"] + ": api_snapshot_target requires a selected API and a literal target triple")
        targets[package["name"]] = target
    return targets


def main() -> int:
    if sys.argv[1:] not in ([], ["--targets"]):
        print("usage: public_api_selection.py [--targets] < Cargo-metadata-json", file=sys.stderr)
        return 2
    try:
        packages = json.load(sys.stdin)["packages"]
        selected, _ = selected_crates(packages)
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    # Preserve the original Windows UTF-8/LF shell protocol.
    output = ([name + " " + target for name, target in sorted(snapshot_targets(packages).items())]
              if sys.argv[1:] else selected)
    sys.stdout.buffer.write(("\n".join(output) + "\n").encode("utf-8"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
