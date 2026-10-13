"""The existing public-API metadata selection, shared by SDK qualification."""
from __future__ import annotations

import json
import sys


def selected_crates(packages: list[dict]) -> tuple[list[str], list[str]]:
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


def main() -> int:
    if len(sys.argv) != 1:
        print("usage: public_api_selection.py < Cargo-metadata-json", file=sys.stderr)
        return 2
    try:
        selected, _ = selected_crates(json.load(sys.stdin)["packages"])
    except (ValueError, KeyError, TypeError, AttributeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    # Preserve the original Windows UTF-8/LF shell protocol.
    sys.stdout.buffer.write(("\n".join(selected) + "\n").encode("utf-8"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
