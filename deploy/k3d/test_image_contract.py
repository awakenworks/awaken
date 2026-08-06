#!/usr/bin/env python3
"""Static contract for the combined K3D product runtime image."""

from pathlib import Path


def main() -> None:
    # Cause/effect decision table. C1 the product Pod executes rooted Bash tools;
    # C2 bundled Skills may invoke Python validators; C3 TLS/sandbox primitives
    # remain required; C4 Namespace layout binds workspace and outputs onto image
    # mount points. E1 the one canonical image contains python3, bubblewrap, CA
    # roots and OpenSSL; E2 both mount points exist before either shipped binary is
    # copied. Rules: I1 C1+C2+C3 => E1; I2 C1+C4 => E2.
    dockerfile = (Path(__file__).parent / "Dockerfile").read_text()
    install = dockerfile.split("COPY awaken-server", 1)[0]
    for package in ("bubblewrap", "ca-certificates", "libssl3", "python3"):
        assert package in install, f"K3D product image is missing runtime package: {package}"
    mkdir = next(line for line in install.splitlines() if "mkdir -p" in line)
    for mount_point in ("/workspace", "/outputs"):
        assert mount_point in mkdir, f"K3D product image is missing mount point: {mount_point}"
    # I3 Coordinator and Worker are separate deployables -> the combined
    # black-box fixture image must copy those exact artifacts, not route either
    # role through the management CLI auxiliary slot.
    assert "ARG AUXILIARY_EXECUTABLE=awaken-control" in dockerfile
    assert "COPY ${AUXILIARY_EXECUTABLE} /usr/local/bin/${AUXILIARY_EXECUTABLE}" in dockerfile
    assert "COPY awaken-coordinator /usr/local/bin/awaken-coordinator" in dockerfile
    assert "COPY awaken-worker /usr/local/bin/awaken-worker" in dockerfile
    resources = (Path(__file__).parent / "distributed-control" / "resources.yaml").read_text()
    assert '/usr/local/bin/awaken-control", "database", "migrate"' in resources
    assert '/usr/local/bin/awaken-control", "--config"' in resources
    assert '/usr/local/bin/awaken-coordinator", "database", "migrate"' in resources
    assert '/usr/local/bin/awaken-coordinator", "--config"' in resources
    assert '/usr/local/bin/awaken", "coordinator"' not in resources
    assert '/usr/local/bin/awaken", "database", "migrate"' not in resources
    assert "AWAKEN_MODEL_MODE, value: distributed-control" not in resources
    worker_image = (
        Path(__file__).parent.parent / "images" / "worker" / "Dockerfile"
    ).read_text()
    # I4 the standalone production image owns only the Worker artifact, runs as
    # the fixed non-root identity, and carries the Namespace sandbox runtime.
    assert "COPY ${BIN} /usr/local/bin/awaken-worker" in worker_image
    assert 'ENTRYPOINT ["/usr/local/bin/awaken-worker"]' in worker_image
    assert "USER 10001" in worker_image
    assert "bubblewrap" in worker_image
    assert "/usr/local/bin/awaken\n" not in worker_image
    # Cause/effect rule I5: selecting a Control or Coordinator production image
    # yields exactly its role-named entry executable; the container cannot switch
    # authority by supplying another aggregate CLI subcommand.
    for role in ("control", "coordinator"):
        role_image = (
            Path(__file__).parent.parent / "images" / role / "Dockerfile"
        ).read_text()
        assert f"COPY ${{BIN}} /usr/local/bin/awaken-{role}" in role_image
        assert f'ENTRYPOINT ["/usr/local/bin/awaken-{role}"]' in role_image
        assert "USER 10001" in role_image
        assert " /usr/local/bin/awaken\n" not in role_image
    # Cause/effect decision table. C5 Docker uses a non-default Buildx driver;
    # C6 the production-image acceptance command immediately runs the tagged
    # image. E3 the build explicitly loads its result into Docker's image store.
    # Rule I5: C5+C6 => E3; Podman keeps its ordinary engine-owned output path.
    sandbox_build = (
        Path(__file__).parent.parent / "images" / "sandbox" / "build.sh"
    ).read_text()
    assert 'if [[ "${engine##*/}" == "docker" ]]' in sandbox_build
    assert (
        'run_with_deadline "$build_timeout_seconds" "$engine" build --load "$@"'
        in sandbox_build
    )
    assert (
        'run_with_deadline "$build_timeout_seconds" "$engine" build "$@"'
        in sandbox_build
    )
    assert sandbox_build.count("  build_image ") == 2
    # C7 the catalog changes package/argv/auth facts; C8 Docker consumes an ACP
    # contract. E4 build.sh generates the contract from the Rust catalog and
    # passes that exact ephemeral file to Docker; E5 the installer/verifier consume
    # all generated requirements/executables. I6 C7+C8=>E4+E5. Exact row content
    # is owned by the Rust generator test, avoiding a second JSON source of truth.
    sandbox = Path(__file__).parent.parent / "images" / "sandbox"
    sandbox_dockerfile = (sandbox / "Dockerfile").read_text()
    assert "--example image_runtime_contract" in sandbox_build
    assert sandbox_build.count("--build-arg ACP_RUNTIME_CONTRACT=") == 2
    assert "ARG ACP_RUNTIME_CONTRACT=" in sandbox_dockerfile
    assert "COPY ${ACP_RUNTIME_CONTRACT}" in sandbox_dockerfile
    assert not (sandbox / "acp-runtimes.json").exists()
    installer = (sandbox / "install-acp-runtimes.py").read_text()
    verifier = (sandbox / "verify-acp-runtimes.py").read_text()
    for source in (installer, verifier):
        assert 'runtime["executables"]' in source
    assert 'runtime["requirements"]' in installer
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


if __name__ == "__main__":
    main()
