# ADR-0092: Worker Capacity and Schedulability

- Status: Accepted
- Date: 2026-10-09
- Revised: 2026-10-10 (simplified after review: Kubernetes keeps judging headroom; the
  first draft's capacity domain, quota-derived capacity, backpressure observation and
  RBAC additions are withdrawn, see Alternatives)
- Builds on: [ADR-0065](0065-recoverable-embeddable-remote-worker.md) (the validated
  registration manifest owns execution capacity; `RegisteredCapacity` sizes the pool);
  [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md)
  (capability and capacity are separate placement questions);
  [ADR-0089](0089-session-realization-single-owner.md) (the realization executor reports
  observations, the Session aggregate decides);
  [ADR-0091](0091-architecture-rules.md) D1 and D5 (one decision point per state
  machine; across processes, capabilities are a manifest, attestation and observation).
- Amends: [ADR-0065](0065-recoverable-embeddable-remote-worker.md) Decision, capacity
  paragraph (the CPU-derived default no longer applies to a Kubernetes Worker);
  [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md)
  2026-08-10 amendment (Kubernetes, the resource-feasibility authority, now answers
  "no capacity" with a typed refusal that returns the claim).
- Affects: `awaken-sandbox-container` (Kubernetes realization wait and error
  classification), `awaken-provisioning-contract` (`SandboxError`), `awaken-runtime-host`
  (Worker claim projection of realization failures), `awaken-worker` (start-up
  validation and probe order), `awaken-sandbox-local`, `awaken-execution-composition`.

## Context

A Worker advertises three layers of capability: the immutable `WorkerManifest`
(fingerprinted, including `capacity.max_concurrent`), run-time attestation (the
Kubernetes adapter proves NetworkPolicy before claiming network isolation,
`awaken-sandbox-container/src/k8s/network_policy.rs:85`), and dynamic heartbeat
observations outside the fingerprint (`in_flight`, warm Environment shapes, credential
and ACP observations). Placement filters with `WorkerSnapshot::accepts`
(`awaken-worker-contract/src/registry.rs:782`, which requires
`in_flight < max_concurrent`) and ranks with `LeastLoadedPolicy`. A Kubernetes Worker
runs one Pod per sandbox; `RegisteredCapacity` guarantees that registered and installed
slots agree. Measured on main `9db2ceeac7`, five gaps remain.

**Capacity comes from the wrong machine.** Without `worker_max_concurrent`,
`StandardManifestConfig::default()` sets `max_concurrent` to the Worker process's
`available_parallelism()` (`awaken-worker/src/manifest.rs:86`). For Local, Namespace,
Docker and Podman the sandboxes run on that host, so the number is at least related to
the resource they consume. A Kubernetes sandbox consumes cluster resources bounded by
node inventory and the namespace `ResourceQuota`; the Worker Pod's CPU count says
nothing about either. The shipped k3d Worker configurations already set it explicitly
(`deploy/k3d/product-backend/resources.yaml:53`,
`deploy/k3d/distributed-control/resources.yaml:147`).

**An unschedulable Pod is a 120-second timeout.** `await_pod_ready`
(`k8s/realization.rs:408`) fails fast only on container-level reasons (image pull,
create and crash errors). A Pod the scheduler cannot place stays `Pending` with
`PodScheduled=False, reason=Unschedulable`, which the readiness function reports as
"Pod phase is Pending" until `POD_READY_TIMEOUT` (120 s, `k8s/realization.rs:18`)
expires and the realization fails as a `Backend` error. The slot stays occupied for the
whole wait, and the failure looks like any other realization failure. 120 s is also
shorter than a cluster autoscaler typically needs to add a node, so a cluster that
would have made room fails the Run first.

**Over-quota is reported as a refusal of authority.** A Pod Create refused by the
namespace `ResourceQuota` admission fails synchronously with HTTP 403 and the message
`exceeded quota: …`. `RuntimeError::from_api_status` (`runtime_error.rs:60`) maps every
401 and 403 to `Unauthorized`, whose disposition is `Permanent`
(`awaken-provisioning-contract/src/sandbox/error.rs:163`), so a full namespace looks
like a missing permission. Both create transports carry the API `Status` unchanged:
the direct client, and the Coordinator broker (`/v1/worker/k8s/create`), which
forwards upstream 400/401/403/409/422 bodies as received
(`awaken-coordinator/src/worker_transport/kubernetes/create_broker.rs:615`) and
synthesizes its own 403 `Forbidden` only for a Worker that is no longer current
(`create_broker.rs:558`).

**Workers sharing a namespace add up.** Each Worker in the same cluster and namespace
registers its own `max_concurrent`; placement sums them. Nothing reads the
`ResourceQuota`, and there is no shared view of what the namespace can still hold.
ADR-0073's 2026-08-10 amendment delegated feasibility to Kubernetes when the per-sandbox
ceiling is omitted, but gave Kubernetes no way to answer back.

**Local backends partly trust the configured tier.** Verified in code:

| Backend | Start-up probe today | Capability source |
|---|---|---|
| Local (Workdir) | None. `resolve_sandbox_tier` passes every tier except Namespace through unprobed (`awaken-execution-composition/src/local.rs:17`, test `resolve_sandbox_tier_passes_non_namespace_tiers_through_unprobed`). `LocalProvider::probe_ready` exists (`awaken-sandbox-local/src/provider.rs:1033`) but the Worker's start-up probe and heartbeat monitor run only for an installed container provider (`awaken-worker/src/lib.rs:1629`, `:1713`; `lifecycle.rs:374`). TrustedHost scoped execution is probed when installed (`awaken-session-environment/src/provider.rs:152`). | Tier projection (`workdir_capabilities`) |
| Namespace (bwrap / Seatbelt) | Real probe: runs the full unprivileged profile (`awaken-sandbox-local/src/namespace.rs:665`) and `awaken-sandbox hand --check` (`local.rs:71`). With `allow_local_fallback` it runs before registration and the manifest uses the resolved tier (`lib.rs:731`). Without it, the manifest is derived from the configured tier at build (`manifest.rs:213` → `awaken-deployment-config/src/lib.rs`), the Worker registers (`lib.rs:1443`), and only then does `with_session_environment_from_deployment` probe (`lib.rs:1657`). Not re-probed by the heartbeat. | Tier projection |
| Docker, Podman | The client is built without I/O (`Docker::connect_with_local_defaults`, `docker.rs:539`). After registration and before Ready the Worker calls `probe_ready`, a daemon `version` ping (`docker.rs:939`, `podman.rs:686`; `lib.rs:1713`), and repeats it on the heartbeat (`lifecycle.rs:374`). | Static adapter answers: `enforces_network_none` and `supports_package_provisioning` return `true` (`docker.rs:943`, `:947`; `podman.rs:690`, `:694`); `resource_limits` is always `true` (`runtime.rs:295`). The ping proves the daemon answers, not that the image resolves, that limits are enforceable (rootless Podman without cgroup delegation) or that the package builder works. |
| Kubernetes | NetworkPolicy attestation before network isolation is claimed and on every probe (`k8s/runtime_contract.rs:420`, `network_policy.rs:57`). | Attested; package provisioning only with a configured registry and builder (`awaken-deployment-config/src/lib.rs`). |

A Worker is not placeable before its first Ready heartbeat, so the unproven Namespace
manifest cannot receive work; but it sits in the registry until its lease expires, and
Workdir, Docker and Podman advertise capabilities no probe established.

The dispatch side already has the return path this record needs. A claim whose
resolution reports pressure rather than a failure is deferred with `defer_claim`
(since 2026-10-18 a classified failed attempt instead spends one attempt through
`retry_claim`, ADR-0015 amendment)
(`awaken-dispatch-execution/src/pool.rs:1201`): the exact claim returns to pending until
a store-owned deadline, with request bytes, priority, retry budget and any physical
attempt slot preserved (`run-ingress-contract/src/dispatch/queue.rs:230`), and the pool
treats it as request-local pressure that does not trip its Worker-wide error gate
(`pool.rs:850`). A remote Worker reaches the same operation through the relinquish
endpoint with `retry_after_ms`
(`relinquish` in `awaken-dispatch-http/src/worker_dispatch/run_dispatch_endpoints.rs`). The
Session Work busy case uses it the same way (`bind_claimed_session_work_authority` in
`awaken-dispatch-http/src/worker_dispatch.rs`).

## Decision

Kubernetes stays the only judge of cluster headroom. Awaken does not model the
cluster; it recognizes Kubernetes' two "no room" answers, gives the cluster a bounded,
configurable time to make room, and when the answer is still no, hands the claim back
so any eligible Worker may take it. Whoever meets the hard refusal yields.

1. **An over-quota 403 is `CapacityUnavailable`, not `Unauthorized`.** `SandboxError`
   gains `CapacityUnavailable(String)`, with disposition `Transient` (it can clear on
   another Worker or after the quota is raised). The Kubernetes classification maps a
   Create answered with 403 whose `Status` message carries the quota admission's
   `exceeded quota` marker to `CapacityUnavailable`. Every other 403 keeps
   `Unauthorized`: RBAC refusals, the broker's own `Forbidden` for a stale Worker, a
   quota that rejects a Pod for missing requests (`failed quota: … must specify`), and
   `LimitRange` refusals. The rule is one pure function of status and message, shared
   by the direct and brokered create transports, which already carry the `Status`
   unchanged. A refused Create has no API effect, so nothing needs disposal.

2. **An unschedulable Pod waits up to a configurable maximum, then is
   `CapacityUnavailable`.** `await_pod_ready` distinguishes "not yet scheduled" from
   "scheduled, starting". While the Pod is `Pending` with `PodScheduled=False,
   reason=Unschedulable`, the wait is bounded by a Kubernetes sandbox setting,
   `k8s_unschedulable_wait_secs` (beside `k8s_namespace`), default 300 s so a cluster
   autoscaler can add a node; the bound is measured from Pod creation. When it expires
   the realization fails with `CapacityUnavailable`, carrying the scheduler's message,
   and the realization executor deletes the Pending Pod through the existing UID-fenced disposal.
   Once the Pod is scheduled, the existing `POD_READY_TIMEOUT` bounds image pull,
   container start and Hand readiness; it never measures waiting for capacity.
   Container-level fast failures are unchanged. The Worker slot stays occupied for
   the whole wait, so `in_flight` keeps telling the truth.

3. **`CapacityUnavailable` returns the claim without consuming the retry budget, and
   placement may pick another Worker.** The realization executor reports the failure as an
   observation; the Session aggregate records it as a realization that produced no
   environment, adding no Session state (ADR-0089 decides, the realization executor does not). The
   Worker's projection of realization failures maps `CapacityUnavailable` to the
   existing request-local deferral, never to `TerminalResolution`: `defer_claim` with
   the pool's bounded claim-failure backoff returns the exact claim to pending, keeps
   its retry budget, and leaves the Worker-wide error gate untouched. After the
   deadline the claim is ordinary pending work: placement filters and ranks as today,
   so a Worker in another namespace or cluster, or the same Worker once the quota is
   raised or a node is added, can take it. No Worker carries a "cluster full" flag;
   the next attempt asks Kubernetes again.

4. **On Kubernetes, `max_concurrent` is configured explicitly.** A Worker whose
   sandbox backend is Kubernetes refuses to start when `worker_max_concurrent` is not
   set, with a message that names the setting and says it bounds simultaneous sandbox
   Pods. The value goes into the immutable manifest as today; `RegisteredCapacity` is
   unchanged. The CPU-derived default remains only for backends whose sandboxes run
   on the Worker's own host (Local, Namespace, Docker and Podman on a local socket),
   and for the unregistered in-process pool (`DispatchPoolSizing::LocalParallelism`),
   which ADR-0065 keeps and which has no capacity setting to require. Workers that
   share a namespace may still sum to more than the quota holds; decisions 1–3 make
   that safe, because Kubernetes refuses the excess and the claim moves on.

5. **Every backend proves at start-up, before registration, each capability it
   advertises.**
   - The Worker constructs and probes the Session provider before deriving its
     standard manifest, for every tier: the Namespace probe moves ahead of
     registration with or without `allow_local_fallback`, and the existing
     post-registration comparison of manifest and installed provider
     (`manifest::ensure_registered_local_provider`, `lib.rs:1671`) covers every local
     tier.
   - Workdir probes that its provider base is a usable directory it owns (the
     existing `LocalProvider::probe_ready`, now called at start-up).
   - Docker and Podman probe, besides the daemon ping: that the configured default
     image resolves; that the daemon enforces CPU, memory and pid limits (cgroup
     controllers reported by `info`), else `resource_limits` is withheld; and that the
     package builder is reachable, else `package_provisioning` is withheld.
   - A capability whose probe fails is withheld, not assumed; the Worker refuses to
     start only when the deployment requires that capability (for example a
     configured `network none` posture) or when no isolation tier remains.
   - All local backends join the existing heartbeat readiness monitor, so a backend
     lost after start drains the Worker as a lost container daemon does today.

## Alternatives

- **One Worker per namespace, with a `capacity_domain` in the manifest.** Rejected:
  it adds a manifest field, a registry admission rule and a restart pause per namespace
  only to prevent over-commitment that decisions 1–3 already make harmless.
- **A Coordinator quota view (shared accounting of live sandbox requests per
  namespace).** Rejected: a second copy of what the quota controller already decides,
  with its own reconcile loop for Pods created outside Awaken, terminating Pods and
  quota edits.
- **Capacity derived from the `ResourceQuota` at start-up.** Rejected: it needs new
  RBAC on `resourcequotas`, goes stale when the quota changes, says nothing about node
  inventory, and still cannot stop concurrent Workers from racing; an explicit number
  plus the refusal path is simpler and exact.
- **A public "waiting for resources" Run or Session state.** Rejected: waiting is
  bounded and internal to realization, and a returned claim is already visible as
  pending queue work; a new public state would change wire shapes for no new decision.
- **Releasing the Worker slot while the Pod waits.** Rejected: the Pod and the claim
  are still held, so a released slot would let `in_flight` under-count and admit more
  Pods into the cluster that is already full.
- **Keep the CPU-derived default for Kubernetes and document `worker_max_concurrent`.**
  Rejected: the default stays wrong by construction; documentation does not stop an
  unconfigured Worker from guessing.
- **Shorten or keep a single `POD_READY_TIMEOUT`.** Rejected: it mixes waiting for
  capacity with starting a scheduled Pod; shorter fails slow image pulls, the current
  120 s fails before an autoscaler can act.
- **Refuse to start whenever any probe fails.** Rejected: too coarse; a Docker host
  without cgroup delegation can still serve Environments that request no limits.

## Tests and formal verification

Existing tests stay or move with their code: the `k8s/realization.rs` readiness
decision table, the `runtime_error` API-status classification tables
(`api_status_classification_tests`), the `SandboxError` construction-site table, the
Worker resolver's realization-failure tables (`realization_failure.rs`), and the
placement eligibility tests. New tests, each written as a cause → effect table first:

| Area | Rows |
|---|---|
| Create classification | 403 with `exceeded quota` → `CapacityUnavailable`; 403 from RBAC, the broker's `Forbidden`, `failed quota: … must specify` or `LimitRange` → `Unauthorized`; 401 → `Unauthorized`; 404, 409 and other statuses unchanged; the direct and brokered transports agree on every row |
| Schedulability wait | Unschedulable within the bound → keep waiting; Unschedulable past the bound → `CapacityUnavailable` and the Pending Pod is disposed; scheduled → `POD_READY_TIMEOUT` applies; container fast-failure reasons unchanged; bound `0` → the first Unschedulable observation fails |
| Claim projection | `CapacityUnavailable` → request-local `defer_claim`, retry budget unchanged, error gate untouched, never `TerminalResolution`; a stale claim is fenced; other classes keep their projection |
| Dispatch state machine | a deferred claim becomes claimable after its deadline by another eligible Worker; the attempt count after N capacity deferrals equals the count before |
| Start-up | a Kubernetes Worker without `worker_max_concurrent` refuses to start (fail closed); with it, the manifest carries the value; local and container backends withhold a capability whose probe fails and refuse to start only when that capability is required |
| e2e (k3d, real binaries) | a Run in an over-quota namespace moves to a second Worker in another namespace, or completes on the same Worker after the quota is raised; an unschedulable Pod past the bound returns the claim instead of failing the Run |

Formal: the existing obligations
`run_ingress.relinquish_requires_exact_current_claim_and_preserves_retry_budget` and
`run_ingress.request_local_resolution_retry_honors_persisted_deadline` cover the
return path and are extended to name `CapacityUnavailable` as a caller; the Kani
harness for
`session_realization.control_failure_disposition_and_current_owner_loss_are_total_exact_and_terminal_absorbing`
stays total over any new projection row. The classification and the schedulability
decision are finite pure functions and get exhaustive decision tables. Run
`scripts/ci/check_formal_surface.py --staged`.

## Consequences

- A Kubernetes Worker without `worker_max_concurrent` no longer starts; the shipped
  k3d configurations already set it. No RBAC change: the classification reads the
  Create response and the Pod status the adapter already reads.
- A full namespace no longer fails Runs: an over-quota Create returns the claim at
  once, and an unschedulable Pod returns it after the configured bound, which gives an
  autoscaler time to act. Placement may move the work to another Worker.
- `SandboxError` gains `CapacityUnavailable`. The enum is not serialized and not
  `#[non_exhaustive]`, so the change is source-level only: exhaustive matches gain an
  arm (awaken-cloud only constructs `SandboxError` values and tests them with
  `matches!`, so it needs no change). The Worker manifest, heartbeat, snapshot and public Run and Session states
  are unchanged. The deployment configuration gains one optional Kubernetes setting
  with a default.
- Every advertised capability is backed by a probe, so a manifest no longer claims
  limits, images or builders the host cannot deliver.
- ADR-0065's capacity paragraph and ADR-0073's 2026-08-10 amendment carry pointers to
  this record.
- Implementation and tests are work item W31.


## 2026-10-18 implementation ownership amendment

The existing container failure enum, including `CapacityUnavailable`, now
belongs to `awaken-container-contract::RuntimeError`; the former Platform
`awaken_sandbox_container::RuntimeError` path is removed without an alias.
The seven variants, display strings, post-mutation exceptions and the unique
conversion to provisioning `SandboxError` keep their existing behavior. The
shared Container API status projection accepts only an extracted `u16` status
and detail, with no typed transport dependency. Docker and Kubernetes retain
their original local source classifiers; Kubernetes still tests the original
403 quota predicate before the generic status projection. Worker outer broker
HTTP uncertainty, Provider retention/fence refresh, dispatch retry and exact
physical disposal remain under their existing owners. This is a value-owner
migration, not proof that Coordinator's implementation closure is removed.

The contract's existing provisioning edge already reaches `thiserror`; adding
its direct declaration adds no dependency-closure node. Relocating the
unchanged `Backend(String)` variant increases the measured contract catch-all
count by one and requires an explicit canonical ratchet reason. Neither the
SDK surface nor a serialized failure format changes. The two inspected Cloud
source trees have no references to the removed error path; deployment and
uninspected external Platform consumers require their own qualification.
