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
    pvc_command = (
        r'^AWAKEN_K8S_E2E=1 run_required_rust_tests -p awaken-session-realization\s*\\?\s*\n'
        r'\s*--features k8s-tests --lib --locked\s*\\?\s*\n'
        r'\s*cache_volume::tests::k8s_live_pvc_initialization_is_readable_and_reused\s*\\?\s*\n'
        r'\s*--require-case=cache_volume::tests::k8s_live_pvc_initialization_is_readable_and_reused\s*\\?\s*\n'
        r'\s*-- --exact$'
    )
    if (
        not re.search(r'^source scripts/ci/_rust_test_execution\.sh$', gate, re.MULTILINE)
        or len(re.findall(pvc_command, gate, re.MULTILINE)) != 1
        or re.search(r'^\s*PVC_TESTS?=', gate, re.MULTILINE)
        or re.search(r'\bcargo test\b[^\n]*-p awaken-session-realization\b', gate)
    ):
        errors.append("Kubernetes gate must select and execute the exact live PVC test in its current owner")
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
    # distributed gate exact-selects the current process-root test. Effects: C17+C18
    # rejects duplicate/zero-selected proof; !C17+C18 retains one live owner.
    # Its existing contract and live gate own C18, so this case mutates C17.
    assert k8s_container_gate_errors(
        gate
        + "\ncargo test -p awaken-cli --features container-k8s --lib "
        "real_pod_identity_survives_reopen_and_independent_stop_without_false_retirement\n"
    ), "K3 stale CLI Worker-stop filter must not qualify the container gate"

    # K4 decision table: current owner + full case/filter + required floor +
    # k8s-tests + live switch + exact libtest argv + the sourced canonical
    # executor => qualify this composition. Changing any one rejects it.
    # The old list-exact cause is now the same wrapper argv's --exact; the
    # old grep-floor cause is --require-case. The unchanged executor owns
    # SAME-argv listing/running, named success, summary and skip refusal.
    # A second manual selector or duplicate invocation must not survive this
    # consolidation. K1-K3 still own the independent control/Worker rules.
    for mutation in (
        gate.replace('-p awaken-session-realization', '-p awaken-session-executor'),
        gate.replace('  cache_volume::tests::', '  old::tests::', 1),
        gate.replace('--features k8s-tests --lib', '--lib'),
        gate.replace('-- --exact', '--'),
        gate.replace('  --require-case=cache_volume::tests::k8s_live_pvc_initialization_is_readable_and_reused \\\n', ''),
        gate.replace('AWAKEN_K8S_E2E=1 run_required_rust_tests -p awaken-session-realization',
                     'AWAKEN_K8S_E2E=0 run_required_rust_tests -p awaken-session-realization'),
        gate.replace('source scripts/ci/_rust_test_execution.sh\n', ''),
        gate.replace('run_required_rust_tests -p awaken-session-realization',
                     'unqualified_run_required_rust_tests -p awaken-session-realization'),
        gate + '\nPVC_TEST=cache_volume::tests::k8s_live_pvc_initialization_is_readable_and_reused\n',
        gate + '\ncargo test --locked -p awaken-session-realization --features k8s-tests --lib stale\n',
        gate + '\n' + gate[gate.index('AWAKEN_K8S_E2E=1 run_required_rust_tests'):gate.index('# Pod-bound')],
    ):
        assert k8s_container_gate_errors(mutation), "K4 live PVC proof cannot silently disappear"
