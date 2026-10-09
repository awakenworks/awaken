# ADR-0092: Worker Capacity and Schedulability

- Status: Proposed
- Date: 2026-10-09
- Builds on: [ADR-0065](0065-recoverable-embeddable-remote-worker.md) (the validated
  registration manifest owns execution capacity; `RegisteredCapacity` sizes the pool);
  [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md)
  (capability and capacity are separate placement questions);
  [ADR-0084](0084-shared-worker-transport-assembly.md) (the Worker registry is the
  decision point for incarnations); [ADR-0089](0089-session-realization-single-owner.md)
  (the realizer reports observations, the Session aggregate decides);
  [ADR-0091](0091-architecture-rules.md) D1 and D5 (one decision point per state
  machine; across processes, capabilities are a manifest, attestation and observation).
- Amends: [ADR-0065](0065-recoverable-embeddable-remote-worker.md) Decision, capacity
  paragraph (the CPU-derived default and "no second capacity configuration");
  [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md)
  2026-08-10 amendment (Kubernetes as the resource-feasibility authority now reports
  back to placement).
- Affects: `awaken-worker` (manifest derivation, start-up order), `awaken-worker-contract`
  (`WorkerManifest`, `WorkerHeartbeat`, `WorkerSnapshot::accepts`),
  `awaken-sandbox-container` (Kubernetes realization and error classes),
  `awaken-sandbox-local`, `awaken-execution-composition`, the Kubernetes RBAC base.

## Context

A Worker advertises three layers of capability: the immutable `WorkerManifest`
(fingerprinted, including `capacity.max_concurrent`), run-time attestation (the
Kubernetes adapter proves NetworkPolicy before claiming network isolation,
`awaken-sandbox-container/src/k8s/network_policy.rs:85`), and dynamic heartbeat
observations outside the fingerprint (`in_flight`, warm Environment shapes, credential
and ACP observations). Placement filters with `WorkerSnapshot::accepts`
(`awaken-worker-contract/src/registry.rs:781`, which requires
`in_flight < max_concurrent`) and ranks with `LeastLoadedPolicy`. A Kubernetes Worker
runs one Pod per sandbox; `RegisteredCapacity` guarantees that registered and installed
slots agree. Measured on main `75de0b9ea3`, four gaps remain.

**Capacity comes from the wrong machine.** Without `worker_max_concurrent`,
`StandardManifestConfig::default()` sets `max_concurrent` to the Worker process's
`available_parallelism()` (`awaken-worker/src/manifest.rs:86`). For Local, Namespace,
Docker and Podman the sandboxes run on that host, so the number is at least related to
the resource they consume. A Kubernetes sandbox consumes cluster resources bounded by
node inventory and the namespace `ResourceQuota`; the Worker Pod's CPU count says
nothing about either.

**An unschedulable Pod is a 120-second timeout.** `await_pod_ready`
(`k8s/realization.rs:408`) fails fast only on container-level reasons (image pull,
create and crash errors). A Pod the scheduler cannot place stays `Pending` with
`PodScheduled=False, reason=Unschedulable`, which the readiness function reports as
"Pod phase is Pending" until `POD_READY_TIMEOUT` (120 s, `k8s/realization.rs:18`)
expires and the realization fails as a backend error. The slot stays occupied for the
whole wait, nothing tells placement that this Worker's cluster is full, and the failure
looks like any other realization failure. A Pod Create refused by the namespace quota
fails synchronously with HTTP 403, which `RuntimeError::from_api_status`
(`runtime_error.rs:60`) classifies as `Unauthorized` — an authority refusal, not a
capacity signal.

**Workers sharing a namespace add up.** Each Worker in the same cluster and namespace
registers its own `max_concurrent`; placement sums them. Nothing reads the
`ResourceQuota`, and there is no shared view of what the namespace can still hold.
ADR-0073's 2026-08-10 amendment delegated feasibility to Kubernetes when the per-sandbox
ceiling is omitted, but gave Kubernetes no way to answer back.

**Local backends partly trust the configured tier.** Verified in code:

| Backend | Start-up probe today | Capability source |
|---|---|---|
| Local (Workdir) | None. `resolve_sandbox_tier` passes every tier except Namespace through unprobed (`awaken-execution-composition/src/local.rs:17`, test `resolve_sandbox_tier_passes_non_namespace_tiers_through_unprobed`). `LocalProvider::probe_ready` exists (`awaken-sandbox-local/src/provider.rs:1033`) but the Worker's start-up probe and heartbeat monitor run only for an installed container provider (`awaken-worker/src/lib.rs:1627`, `:1719`; `lifecycle.rs:371`). TrustedHost scoped execution is probed when installed (`awaken-session-environment/src/provider.rs:152`). | Tier projection (`workdir_capabilities`) |
| Namespace (bwrap / Seatbelt) | Real probe: runs the full unprivileged profile (`awaken-sandbox-local/src/namespace.rs:665`) and `awaken-sandbox hand --check` (`local.rs:56`). With `allow_local_fallback` it runs before registration and the manifest uses the resolved tier (`lib.rs:730`). Without it, the manifest is derived from the configured tier at build (`manifest.rs:212` → `deployment_config.rs:629`), the Worker registers (`lib.rs:1441`), and only then does `with_session_environment_from_deployment` probe (`lib.rs:1658`). Not re-probed by the heartbeat. | Tier projection |
| Docker, Podman | The client is built without I/O (`Docker::connect_with_local_defaults`, `docker.rs:539`). After registration and before Ready the Worker calls `probe_ready`, a daemon `version` ping (`docker.rs:939`, `podman.rs:686`; `lib.rs:1719`), and repeats it on the heartbeat (`lifecycle.rs:371`). | Static adapter answers: `enforces_network_none` and `supports_package_provisioning` return `true` (`docker.rs:943`, `podman.rs:690`); `resource_limits` is always `true` (`runtime.rs:279`). The ping proves the daemon answers, not that the image resolves, that limits are enforceable (rootless Podman without cgroup delegation) or that the package builder works. |
| Kubernetes | NetworkPolicy attestation before network isolation is claimed and on every probe (`k8s/runtime_contract.rs:530`). | Attested; package provisioning only with a configured registry and builder (`deployment_config.rs:568`). |

A Worker is not placeable before its first Ready heartbeat, so the unproven Namespace
manifest cannot receive work; but it sits in the registry until its lease expires, and
Workdir, Docker and Podman advertise capabilities no probe established.

## Decision

1. **A Kubernetes Worker's capacity is configured or derived from the namespace quota,
   never from its own CPU count.** At start-up the Kubernetes backend reads the
   namespace's `ResourceQuota` objects. `max_concurrent` is:
   - the explicit `worker_max_concurrent` when set; the Worker refuses to start if a
     quota exists and the explicit value exceeds the quota-derived bound, because an
     advertised slot the namespace cannot hold is an unproven capability (decision 4);
   - otherwise the quota-derived bound: the minimum, over every resource the quota
     limits (`pods`, `requests.cpu`, `requests.memory`, `limits.*`,
     `count/persistentvolumeclaims`), of `hard` divided by the per-sandbox reference
     demand — `WorkerCapacity.resources` when set, else the deployment's default
     sandbox `ResourceRequests`. Resident Hand and continuation-volume objects count
     toward their own resources;
   - otherwise (no quota and no explicit value) start-up fails with a message naming
     both sources. There is no silent default.

   The derived number goes into the immutable manifest like any other
   `max_concurrent`; `RegisteredCapacity` is unchanged. A quota change takes effect
   at the next incarnation; between incarnations, drift is caught by decision 2. The
   CPU-derived default remains only for backends whose sandboxes run on the Worker's
   own host (Local, Namespace, Docker and Podman on a local socket). There is still
   exactly one capacity number per Worker: the quota is read, not authored by Awaken.

2. **An unschedulable sandbox is a typed observation that releases the slot and
   reports backpressure.**
   - The Kubernetes adapter distinguishes two waits. Waiting for a node ends when
     `PodScheduled=True`; it is bounded by a deployment-configured schedulability grace
     (default 10 s, `0` allowed); a `TriggeredScaleUp` event from a cluster
     autoscaler restarts the grace once. `POD_READY_TIMEOUT` then
     applies only to a scheduled Pod's start-up (image pull, container start, Hand
     readiness) and never measures waiting for capacity.
   - A Pod still `Unschedulable` after the grace, and a Pod Create refused with 403
     `exceeded quota` by the `ResourceQuota` admission, become a new
     `SandboxError::CapacityUnavailable { reason, shape }` instead of a backend error
     or `Unauthorized`. 403 responses without the quota marker keep their
     `Unauthorized` class.
   - The realizer deletes the Pending Pod through the existing UID-fenced disposal and
     reports a `CapacityUnavailable` observation. The Session aggregate records it as
     "not realized, nothing to clean up", not as a realization failure (ADR-0089
     decides; the realizer does not).
   - The Worker returns the claim with the existing `defer_claim`
     (`run-ingress-contract/src/dispatch/queue.rs:238`), which preserves the retry
     budget, frees the slot (`in_flight` drops) and publishes a dynamic heartbeat
     observation `capacity_backpressure: { shape, until_ms }`, outside the manifest
     fingerprint like warm Environment shapes.
   - `WorkerSnapshot::accepts` gains one term: a Worker with a current backpressure
     observation for the claim's `SandboxCapacityShapeId` is ineligible for that shape
     until `until_ms`. Another eligible Worker (another namespace or cluster) can then
     take the claim; if none exists the claim waits as pending work, visible as queue
     depth and a capacity metric, not as a failed Run. How long a Run may wait is the
     dispatch queue's existing pending policy; this record adds no deadline.

3. **One Worker per capacity domain; scale by slots.** Two designs were weighed for
   Workers that share one cluster and namespace:

   | | A. One Worker per namespace | B. Shared quota view |
   |---|---|---|
   | Shape | A Kubernetes namespace (with its quota) is one capacity domain owned by one Worker incarnation; capacity grows by raising its slots or adding namespaces. | Many Workers per namespace; the Coordinator, which already brokers every Worker Pod Create (`/v1/worker/k8s/create`), accounts live sandbox requests per namespace against the quota and placement checks domain headroom. |
   | Authority | Kubernetes stays the only resource authority; Awaken keeps one number per Worker. | A second accounting of cluster resources beside the quota controller; must reconcile Pods created outside Awaken, terminating Pods and quota edits. |
   | New state | `capacity_domain` in the manifest; the registry admits one live incarnation per domain. | A durable per-domain aggregate in the Coordinator, a reconcile loop, cross-Worker state in the eligibility kernel. |
   | Availability | A Worker restart pauses new placements in that namespace until the next incarnation registers; claims are durable and recover (ADR-0065). | Survives one Worker's loss without a pause. |
   | Process load | Native run loops of the whole namespace run in one process; bounded by its slots. | Spread over several processes. |

   **Recommendation: A.** It keeps one decision point per state machine (ADR-0091 D1):
   the namespace's capacity has one owner, and Kubernetes remains the resource
   authority ADR-0073 named, instead of the Coordinator maintaining a second copy of
   it. Decision 2 already makes over-commitment safe, so a shared view would only
   save the latency of a rejected create. The availability gap is the existing Worker
   recovery path, not new loss. A deployment that needs more than one process per
   cluster uses more namespaces. B is the fallback if one process per namespace
   proves to be a throughput limit in qualification; it would need its own ADR.

   The Kubernetes backend publishes `capacity_domain = k8s/<namespace UID>` in its
   manifest; the registry rejects a second live incarnation of a different
   `worker_id` in the same domain with a typed registration error. Docker and Podman
   already name a host lease domain for warm capacity (`docker.rs:1758`,
   `podman.rs:1364`); this record does not change them.

4. **Every backend proves at start-up, before registration, each capability it
   advertises.**
   - The Worker constructs and probes the Session provider before deriving its
     standard manifest, for every tier: the Namespace probe moves ahead of
     registration with or without `allow_local_fallback`, and the existing
     post-registration comparison of manifest and installed provider (`lib.rs:1670`)
     covers every local tier.
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

- **Keep the CPU-derived default for Kubernetes and document `worker_max_concurrent`.**
  The default stays wrong by construction; documentation does not stop a cluster from
  being over-committed by every unconfigured Worker.
- **Read the quota's `used` on every claim instead of deriving capacity at start.**
  Turns a placement predicate into a Kubernetes API call per claim and still races
  with concurrent creates; decision 2 handles the race at the point Kubernetes decides.
- **Shorten `POD_READY_TIMEOUT`.** Hides the symptom and fails slow image pulls without
  telling placement anything.
- **Shared quota view (option B above).** Presented and not recommended.
- **Refuse to start whenever any probe fails.** Too coarse: a Docker host without
  cgroup delegation can still serve Environments that request no limits.

## Consequences

- A Kubernetes Worker without explicit capacity and without a quota no longer starts;
  deployments must set one of them. The Kubernetes RBAC base gains `get`, `list` and
  `watch` on `resourcequotas` and `get` on the Worker's namespace.
- Placement sees cluster pressure within the schedulability grace instead of after
  120 s, and can move work to another Worker; a full namespace shows as backpressure,
  not as failed Runs.
- `WorkerManifest` gains `capacity_domain`, `WorkerHeartbeat` and `WorkerSnapshot` gain
  `capacity_backpressure`, `SandboxError` gains `CapacityUnavailable`; all are additive
  wire fields with serde defaults, so older Coordinators and Workers interoperate until
  the registry's single-domain check is switched on.
- Every advertised capability is backed by a probe, so a manifest no longer claims
  limits, images or builders the host cannot deliver.
- ADR-0065's capacity paragraph and ADR-0073's 2026-08-10 amendment carry pointers to
  this record.
- Implementation and tests are work item W31: decision tables for the capacity
  derivation, schedulability classification and eligibility term, and a k3d
  qualification run in which an over-quota namespace defers to a second Worker.
