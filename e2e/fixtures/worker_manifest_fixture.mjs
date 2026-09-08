// One raw registration manifest for external E2E drivers that emulate a
// database-less workdir Worker. Callers own the capability evidence and build
// identity; this fixture owns only the repeated wire shape and applies no
// placement or credential inference.
export function workdirWorkerManifestFixture({ buildDigest, capabilities }) {
  return {
    manifest_version: 1,
    build_digest: buildDigest,
    capabilities: structuredClone(capabilities),
    zone: null,
    architecture: process.arch,
    sandbox: {
      isolation: 'workdir',
      tool_transparent: false,
      path_fidelity: false,
      enforced_readonly: false,
      network_isolation: false,
      secret_egress_substitution: false,
      resource_limits: false,
      custom_rootfs: false,
    },
    sandbox_backends: [],
    dispatch_contract: { min: 1, max: 1 },
    // V3 carries request-relative Session lease receipts. Keep this explicit;
    // worker_manifest_fixture.test.mjs detects drift against the Rust contract.
    runtime_protocol: { min: 3, max: 3 },
    checkpoint_formats: ['stream-v1'],
    capacity: { max_concurrent: 1, resources: {} },
  };
}
