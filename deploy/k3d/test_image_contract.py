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
    # I3 migrations, Coordinator and Worker are separate production deployables ->
    # the combined black-box fixture image must copy those exact artifacts, not
    # route any role through the management CLI auxiliary slot. The long-running
    # Control Pod intentionally uses the scenario adapter asserted by I4 below so
    # only its model-publication dependency is deterministic.
    assert "ARG AUXILIARY_EXECUTABLE=awaken-control" in dockerfile
    assert "COPY ${AUXILIARY_EXECUTABLE} /usr/local/bin/${AUXILIARY_EXECUTABLE}" in dockerfile
    assert "COPY awaken-coordinator /usr/local/bin/awaken-coordinator" in dockerfile
    assert "COPY awaken-worker /usr/local/bin/awaken-worker" in dockerfile
    resources = (Path(__file__).parent / "distributed-control" / "resources.yaml").read_text()
    policy_graph = (
        Path(__file__).parent / "bases" / "sandbox-network-policy" / "resources.yaml"
    ).read_text()

    def policy(name: str) -> str:
        return policy_graph.split(f"name: {name}", 1)[1].split("---", 1)[0]

    # Network-policy cause/effect rules: N1 exact deny plus open-only widening
    # is the one graph attested by K8sRuntime; generic DNS must not select
    # Sandbox Pods; each production overlay imports that graph and grants only
    # read-only observation to its runtime ServiceAccount.
    assert "ingress: []" in policy("awaken-sandbox-default-deny")
    assert "egress: []" in policy("awaken-sandbox-default-deny")
    assert "awaken-egress: open" in policy("awaken-sandbox-open-egress")
    distributed = Path(__file__).parent / "distributed-control"
    product = Path(__file__).parent / "product-backend"
    dns = (distributed / "network-policies.yaml").read_text()
    assert "operator: NotIn, values: [awaken-sandbox]" in dns
    assert 'resources: ["networkpolicies"]' in resources
    assert 'verbs: ["get", "list", "watch"]' in resources
    for overlay in (distributed, product):
        assert "../bases/sandbox-network-policy" in (overlay / "kustomization.yaml").read_text()
    product_resources = (product / "resources.yaml").read_text()
    assert 'resources: ["networkpolicies"]' in product_resources
    assert 'verbs: ["get", "list", "watch"]' in product_resources
    assert '/usr/local/bin/awaken-control", "database", "migrate"' in resources
    assert 'command: ["/usr/local/bin/awaken-server"]' in resources
    assert '/usr/local/bin/awaken-coordinator", "database", "migrate"' in resources
    assert '/usr/local/bin/awaken-coordinator", "--config"' in resources
    assert '/usr/local/bin/awaken", "coordinator"' not in resources
    assert '/usr/local/bin/awaken", "database", "migrate"' not in resources
    # I4 the deterministic Control fixture changes only the publication resolver:
    # the scenario adapter consumes the deployment's one explicit Control config
    # and assembles the canonical split-Control application. The former HOME
    # config mirror would make the command and mounted source disagree.
    assert "AWAKEN_MODEL_MODE, value: distributed-control" in resources
    assert "AWAKEN_SCENARIO_CONFIG, value: /etc/awaken/control.toml" in resources
    assert "/etc/awaken-home" not in resources
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
    # C7 the catalog changes package/argv/auth/version facts; C8 Docker consumes
    # an ACP contract; C9 the Node runtime matrix consumes ids/version probes.
    # E4 build.sh generates the contract from the Rust catalog and passes that
    # exact ephemeral file to Docker; E5 installer/verifier consume all generated
    # requirements/executables; E6 Node executes the same generator and owns no
    # literal ids or version table. Constraints: neither a checked-in JSON file
    # nor a language-specific mirror may become another catalog. Decision rules:
    # I6 C7+C8=>E4+E5; I7 C7+C9=>E6.
    sandbox = Path(__file__).parent.parent / "images" / "sandbox"
    sandbox_dockerfile = (sandbox / "Dockerfile").read_text()
    assert "--example image_runtime_contract" in sandbox_build
    assert sandbox_build.count("--build-arg ACP_RUNTIME_CONTRACT=") == 2
    assert "ARG ACP_RUNTIME_CONTRACT=" in sandbox_dockerfile
    assert "COPY ${ACP_RUNTIME_CONTRACT}" in sandbox_dockerfile
    assert not (sandbox / "acp-runtimes.json").exists()
    runtime_profiles = (
        Path(__file__).parent.parent.parent / "e2e" / "acp_runtime_profiles.mjs"
    ).read_text()
    assert "--example', 'image_runtime_contract'" in runtime_profiles
    assert "ACP_RUNTIME_IDS = Object.freeze([" not in runtime_profiles
    assert "ACP_RUNTIME_VERSION_SPECS = Object.freeze({" not in runtime_profiles
    installer = (sandbox / "install-acp-runtimes.py").read_text()
    verifier = (sandbox / "verify-acp-runtimes.py").read_text()
    for source in (installer, verifier):
        assert 'runtime["executables"]' in source
    assert 'runtime["requirements"]' in installer
    print("OK - K3D product image contains Sandbox, TLS, and Skill runtime dependencies.")


if __name__ == "__main__":
    main()
