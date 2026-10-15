# Sandbox scope closure: irreversible-close primitive qualification

Cross-process convergence for a Kubernetes sandbox scope with no durable handle
was deliberately left out of scope until one question was answered: **can a late
`Create` for a closed scope be rejected at the actual commit point?** Reading a
ConfigMap or lease and then POSTing, a single `404`, and UID-qualified deletion
all fail that test, because each leaves a window in which the original request
still commits. This document records the qualification of a primitive that does
pass, so the cleanup extension can be designed against evidence rather than
assumption. The cleanup extension itself is not implemented here.

## What the scope projection already gives us

Every participant name is a total function of the scope, owned by `names.rs` in
`awaken-sandbox-container`:

| Participant | Name |
| --- | --- |
| Pod | `awaken-{id}` |
| Continuation claim | `awc-{id}` |
| Inline-content ConfigMap | `awaken-{id}-cfg-{i}` |
| Credential Secret | `awaken-{id}-credential-{i}` |
| Cleanup owner label value | `awaken-{id}` |

`id` is `k8s_runtime_id(scope)`, a deterministic encoding of the scope with a
hashed fallback when the encoded form would exceed the DNS label limit. A closed
scope is therefore fully identifiable from the scope alone. No change to the
object projection is required to express closure.

## The primitive that qualifies

### Client dependency qualification

The client and generated resource types use the workspace-owned `kube` and
`k8s-openapi` declarations, so every adapter resolves one compatible version.
The 2026-10-14 dependency update uses kube 4.2 / k8s-openapi 0.28 and the same
serde-saphyr 0.0.29 parser as the State Machine DSL and manifest tests. Original
client inference, TLS, request lifetime, attestation and disposal owners remain
unchanged; no alternate kubeconfig loader or client path is introduced.
The newly optional NetworkPolicy selector supplies no exact-label proof when
absent. An additional egress rule with an absent selector is conservatively
treated as possibly selecting the sandbox, preserving fail-closed attestation.
Control configuration restores the original finite 295-second defaults while
retaining explicit finite overrides; streaming clients retain their original
unbounded response lifetime. Optional Sandbox reads use the original API/error
boundary with complete Failure/NotFound evidence, so a partial Status or a
client parse-failure reconstruction cannot establish object absence.

Generated types target Kubernetes 1.32. The existing 1.31.5 test cluster is
below upstream's current tested range, so source compilation does not renew
the historical live qualification below. The changed client cohort requires
the existing actual cluster tests and full-product receipt before release.

Namespace deletion. Once a namespace carries a `deletionTimestamp`, the in-tree
`NamespaceLifecycle` admission plugin rejects new content in it. The rejection is
evaluated by the apiserver inside the admission chain, before persistence, so
there is no client-side read-then-write window to lose.

Qualified on Kubernetes `v1.31.5+k3s1`. A ConfigMap held by a finalizer keeps the
namespace in `Terminating` so the window is observable, which is also the shape a
retained Awaken participant produces.

| Scenario | Request | Observed result |
| --- | --- | --- |
| Late original request | replay the exact `Create` that was in flight at close | `configmaps "awaken-scope-pod-a" is forbidden: unable to create new content in namespace … because it is being terminated` |
| Partial create | a participant that had never been created yet | same `Forbidden`, for `awaken-scope-late-credential-0` |
| Substitute identity | a different participant name inside the closed scope | same `Forbidden`, for `awc-scope-substitute` |
| Provider restart | no client state participates in the decision | the rejection is server-side, so a restarted provider observes the same `Forbidden` |

Nothing new persisted. After all three rejected creates the namespace still held
only the objects that existed before close.

Irreversibility was probed directly, because a close that can be reopened is not
a close:

| Attempt | Result |
| --- | --- |
| clear `metadata.deletionTimestamp` | patch applied with no change; phase stays `Terminating` |
| re-create the same namespace name | `AlreadyExists: object is being deleted` |
| force `status.phase: Active` | rejected: `may only be 'Terminating' if deletionTimestamp is not empty` |
| create after all three attempts | still `Forbidden` |

## What this costs, and what it does not give

The primitive is namespace-granular, and the managed Kubernetes client currently
projects every scope into one shared `default_namespace`. Qualifying the primitive
therefore also identifies its price: closure by namespace deletion requires
**one namespace per scope**, which is a design change, not a refactor. It touches
namespace provisioning and its latency on the launch path, the RBAC and quota
that must accompany a per-scope namespace, and the `Api::namespaced` call sites
that today receive one shared name.

Two boundaries remain after the primitive is adopted, and neither is closed by it:

- Namespace termination stops *new* content. It does not remove a participant that
  a finalizer still holds; in the qualification the held ConfigMap survived the
  whole `Terminating` window. Releasing retained participants stays with the
  existing cleanup owner, which must still observe each one.
- A scope name cannot be reused until termination completes. That is the desired
  behaviour for closure, but it means name reuse is gated on the namespace
  controller finishing, and a wedged finalizer blocks reuse rather than failing
  open.

## Alternatives considered and rejected

- **ValidatingAdmissionPolicy.** Commit-point and in-tree, but CEL cannot read
  arbitrary cluster state; a policy may consult only the request object and one
  `paramRef`. Expressing "is this scope closed?" needs either one binding per
  closed scope or a single contended list object holding every closed scope.
- **Validating admission webhook.** Can read arbitrary state, but with
  `failurePolicy: Fail` every `Create` in the cluster then depends on Awaken being
  reachable, and with `Ignore` it fails open, which is the property being removed.
- **Owner references and cascading GC.** A late `Create` naming a deleted owner is
  collected asynchronously, not rejected. Eventually consistent, not commit-point.
- **`ResourceQuota` of zero.** Namespace-granular like namespace deletion, but a
  quota can be raised again, so it is reversible.
