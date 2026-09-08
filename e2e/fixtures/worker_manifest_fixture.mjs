// Canonical secret-free evidence for a test driver that stands in for the
// installed Native provider adapter while it claims (but never executes) a
// production-authored seed dispatch. Keep the wire encoding here so fixtures do
// not grow independently ordered copies of the credential capability contract.
export const WORKER_PROVIDER_CREDENTIAL_CAPABILITY =
  'credential-realization.awaken.dev/v2:{"holders":[{"boundary":"worker","trust_domain":"awaken.worker"}],"material_sources":["control_plane_reference"],"realization_kinds":["worker_provider_adapter"],"recipient_bound_envelopes":false,"envelope_required":false}';

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
    // V4 adds whole-claim retirement and typed renewal outcomes to V3's
    // request-relative Session leases. Seed drivers only claim/commit; shipped
    // Workers own execution and final ACK. Keep this reviewed range explicit;
    // worker_manifest_fixture.test.mjs detects drift against the Rust contract.
    runtime_protocol: { min: 4, max: 4 },
    checkpoint_formats: ['stream-v1'],
    capacity: { max_concurrent: 1, resources: {} },
  };
}
