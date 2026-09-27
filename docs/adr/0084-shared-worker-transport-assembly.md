# ADR-0084: Shared Worker Transport Assembly

- Status: Accepted
- Date: 2026-09-28

## Context

The CLI privately assembled Kubernetes Worker authentication, process binding
and independent stop observation, while downstream compositions could only
select the public ports individually. Copying that adapter into Flow would
create competing implementations of the same provider trust domain.

The Coordinator already has explicit authentication, binding and stop ports.
Dispatch owns physical attempt admission and retirement; the Worker Registry
owns incarnation and process-exit facts. No new execution authority is needed.

## Decision

Move the existing `WorkerTransport` assembly and Kubernetes adapters to the
existing `awaken-coordinator` bootstrap package. Keep its fields private and
expose the existing unbound construction and paired Kubernetes construction.
The CLI retains process-role, operating-mode and configuration ownership and
calls that shared constructor. Consumers select the same constructor before
passing its ports to the canonical registered Worker router.

Kubernetes policy shape validation has one implementation beside the constructor;
CLI role/signing-trust exclusivity remains at the process configuration edge.
Authentication does not prove termination. An unbound authenticator retains no
binding or stop capability. Kubernetes observation still verifies immutable Pod
identity and retained process evidence, and independent whole-attempt observation
still uses the existing Native coverage classifier. Unsupported tool coverage
remains Unknown; this extraction does not widen the physical proof scope.

The original Kubernetes implementation and its tests move with their behavior;
the CLI no longer contains a provider implementation. There is no additional
process registry, recovery queue, receipt store or cancellation protocol.

## Alternatives

- A second Flow Kubernetes implementation would duplicate provider validation.
- Independent public setters for authentication and provider sources would
  lose the existing paired construction guarantee.
- Expanding the terminal classifier during this extraction would combine a
  behavior change with relocation and obscure the regression boundary.

## Consequences

Downstream products reuse one provider implementation. CLI remains a configuration
edge; the existing bootstrap package gains the transport assembly export.
Unbound deployments retain their current fail-closed physical recovery limits,
and unsupported tool or remote participants still prevent whole-attempt proof.

## Validation

The moved tests retain exact TokenReview/Pod binding, foreign/replaced identity,
retained exit and complete model-only termination decision tables. Shared
constructor tests cover unbound capability absence and invalid policy rejection
before provider I/O. CLI tests retain role/feature/signing-trust selection rules.
The existing live Kubernetes test reopens the Coordinator and retires only the
exact independently stopped predecessor. It remains distinct from in-process
API transport tests and is executed only with an explicitly selected cluster.

Dependency direction and public API snapshots gate the new bootstrap export.
