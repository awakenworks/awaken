# ADR-0093: Application Origin Is Immutable Provenance, Not Permission

- Status: Accepted
- Date: 2026-10-12
- Amends: [ADR-0066](0066-session-service-binding-and-realization.md)

## Context

An application may need to prove who initiated an interaction without making
Awaken own its users, business scopes, model-consumption policy, or billing.
Session metadata is mutable presentation context; the application grant owns
only protocol/operation/Managed-Session access; the prompt seed owns invocation
timing. None is an authoritative origin-evidence carrier.

## Decision

Reuse unchanged: Session root CAS, create receipts, WorkQueue, Run dispatch,
application grants, frozen publication resolution, and the Runtime projection.
Modify the complete profiled Session command, its private transport, and the
existing creation-intent/baseline compiler. Add only one bounded opaque
`ApplicationOriginBinding` value in `awaken-session-configuration-contract`.
There is no origin registry, new Session store, grant, actor model, or Runtime
permission path.

### Static structure

```text
application validates initiator and exact context
  -> trusted issuer produces purpose-bound, non-bearer evidence
  -> ProfiledSessionCreate / CreateProfiledSessionCommand
  -> SessionCreationIntent.control.application_origin
  -> SessionBaseline.application_origin + domain-separated fingerprint
  -> existing ManagedSessionRepository atomic root creation

application/hosting reader -> exact Session root -> issuer verification
  -> current product permission and commercial policy, outside Awaken
```

The value contains a namespaced source, opaque SHA-256 context fingerprint, and
bounded ASCII evidence. Its constructor/deserializer validates shape, not
signatures, identity, permissions, or product semantics. Debug omits evidence;
it never enters prompts, model input, `SessionRuntimeProjection`, application
grants, or access-token responses.

The complete `FrozenSessionProjection` still carries the canonical baseline to
a Worker; removing the binding there would create another interpretation. The
Worker receives opaque non-bearer data. Hosting consumers read the Coordinator
root, not a Worker-supplied copy. Account and business-scope types stay outside
Awaken contracts.

The existing Workspace-owned Session execution observation/private
`execution-evidence` adapter projects the original optional binding. It reads
the same root and reconstructs no origin from metadata, proof cache, or grant.

### Dynamic behavior and compatibility

The application supplies evidence before the original insert. Finalization
freezes it in the complete baseline; metadata, Resource, MCP-credential, Run,
and application-token commands cannot replace or clear it. Consumers compare
the exact physical Session/publication/context and recheck current policy.
Presence and successful shape validation confer no grant; missing or invalid
required proof cannot select a broader consumer path.

Absence preserves historical bytes and fingerprints. Presence adds a
domain-separated fingerprint over the complete binding, including proof. A
changed proof is not an exact replay. Lost create responses reconcile the
original Session before obtaining another proof. Readers that reject new fields
precede the writer; absence cannot fabricate a historical initiator.

## Consequences

One root owns retained origin and one read projection exposes it. The Session
package has no issuer keys, customer identity interpretation, commercial policy,
or permission engine. Evidence retention/key rotation remain issuer concerns;
shape/readback tests are not claims of product authorization or Hosted delivery.

G42 remains the existing immutable-baseline guardrail. Its enforcers are the
carrier constructor/deserializer, creation-intent finalizer, canonical root
repository, and original execution observation. Tests attach cause/effect rules
for invalid/absent/present/changed values, legacy shape, fingerprints, persisted
readback, metadata patch/clear, exact projection, and no model-context disclosure.
Product query-only authority, trusted issuance and live model/billing acceptance
are separately required.

## Alternatives considered

- Mutable metadata: presentation updates could change the attributed initiator.
- Application grants: customer identity would leak into a deliberately neutral
  protocol/operation/Session-access contract.
- Prompt context: model-visible text cannot authenticate or authorize its user.
- A separate origin registry: duplicates Session identity, retention and replay.
- A partial Worker baseline: creates a second baseline interpretation and fence.
