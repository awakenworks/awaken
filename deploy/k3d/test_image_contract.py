#!/usr/bin/env python3
"""Static contract for the combined K3D product runtime image."""

from pathlib import Path


def main() -> None:
    # Cause/effect decision table. C1 the product Pod executes rooted Bash tools;
    # C2 bundled Skills may invoke Python validators; C3 TLS/sandbox primitives
    # remain required. E1 the one canonical image contains python3, bubblewrap,
    # CA roots and OpenSSL before either shipped binary is copied. Rule I1:
    # C1+C2+C3 => E1; omitting any package fails this deployment contract.
    dockerfile = (Path(__file__).parent / "Dockerfile").read_text()
    install = dockerfile.split("COPY awaken-server", 1)[0]
    for package in ("bubblewrap", "ca-certificates", "libssl3", "python3"):
        assert package in install, f"K3D product image is missing runtime package: {package}"
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


if __name__ == "__main__":
    main()
