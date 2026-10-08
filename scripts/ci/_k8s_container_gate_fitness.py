"""Keep the live Sandbox gate distinct from distributed Worker-stop proof."""

from __future__ import annotations

import re


def k8s_container_gate_errors(gate: str) -> list[str]:
    errors: list[str] = []
    if 'CONTROL_IMAGE="${AWAKEN_K8S_CONTROL_IMAGE:-}"' not in gate or 'k3d_validate_image_digest "$CONTROL_IMAGE"' not in gate:
        errors.append("Kubernetes gate does not require an exact control-image digest")
    if 'CONTROL_IMAGE=$(k3d_image_digest "$SESSION_IMAGE")' not in gate or (
        'build.sh" --ensure-current "$SESSION_IMAGE" ""' not in gate
    ):
        errors.append("Kubernetes gate cannot qualify the current source without an external image override")
    if not re.search(r'^k3d_create_cluster "\$CLUSTER" 0$', gate, re.MULTILINE):
        errors.append("Kubernetes gate must use the nested-node reserve with shared capacity admission")
    if 'k3d_import_storage_images "$CLUSTER" "$FIXTURE_IMAGE" "$SESSION_IMAGE" "$CONTROL_IMAGE"' not in gate:
        errors.append("Kubernetes gate does not import the exact control image")
    if not re.search(
        r'AWAKEN_K8S_E2E=1 AWAKEN_K8S_CONTROL_E2E=1 AWAKEN_K8S_ISOLATION_E2E=1\s*\\?\s*\n'
        r'\s*AWAKEN_K8S_CONTROL_IMAGE="\$CONTROL_IMAGE"',
        gate,
    ):
        errors.append("Kubernetes gate can still skip the required control roundtrip")
    if "real_pod_identity_survives_reopen" in gate:
        errors.append(
            "Kubernetes container gate duplicates the distributed-control Worker stop test"
        )
    return errors


def self_test(gate: str) -> None:
    # K1/K2 causes: C12 exact control digest; C13 imported image; C14 both
    # live gates plus the same physical Create-isolation case; C15 current-source build and manifest digest; C16 shared
    # nested-node capacity preflight. All true -> helper/marker executes;
    # any absent -> reject false-green, stale-image or eviction qualification.
    assert not k8s_container_gate_errors(gate), "K1 required control roundtrip"
    for mutation in (
        gate.replace('k3d_validate_image_digest "$CONTROL_IMAGE"', 'true', 1),
        gate.replace('CONTROL_IMAGE=$(k3d_image_digest "$SESSION_IMAGE")', 'CONTROL_IMAGE=old:tag', 1),
        gate.replace('build.sh" --ensure-current "$SESSION_IMAGE" ""', 'build.sh" --ensure-hand "$SESSION_IMAGE" ""', 1),
        gate.replace('k3d_create_cluster "$CLUSTER" 0', 'k3d_create_cluster "$CLUSTER" 0 2', 1),
        gate.replace(' "$CONTROL_IMAGE"\n', '\n', 1),
        gate.replace('AWAKEN_K8S_CONTROL_E2E=1', 'AWAKEN_K8S_CONTROL_E2E=0', 1),
        gate.replace('AWAKEN_K8S_ISOLATION_E2E=1', 'AWAKEN_K8S_ISOLATION_E2E=0', 1),
    ):
        assert k8s_container_gate_errors(mutation), "K2 false-green control gate"

    # K3 causes: C17 old CLI Worker-stop filter present/absent; C18 the
    # distributed gate exact-selects the Coordinator test. Effects: C17+C18
    # rejects duplicate/zero-selected proof; !C17+C18 retains one live owner.
    # Its existing contract and live gate own C18, so this case mutates C17.
    assert k8s_container_gate_errors(
        gate
        + "\ncargo test -p awaken-cli --features container-k8s --lib "
        "real_pod_identity_survives_reopen_and_independent_stop_without_false_retirement\n"
    ), "K3 stale CLI Worker-stop filter must not qualify the container gate"
