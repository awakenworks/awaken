# Sandbox provider contract

`awaken-provisioning-contract::SandboxProvider` is the only physical Sandbox
supply port. The container-only `ContainerEnvironmentProvider` projection is
removed rather than retained as an alias or a second implementation. The
associated `Environment` role preserves what the consumer needs: shared
`Sandbox` for filesystem providers, shared `SessionSandbox` for container
providers and their existing attempt/checkpoint participants. Readiness-only
callers use `SandboxReadiness`; it reports capability and liveness and never
creates, adopts, disposes, or supplies a parallel provider authority.

Creation has one synchronous `claim_for_effect(spec, effect, intent)` edge.
Its `ClaimedEffect` retains responsibility before the first poll, for both
unattached capacity and an aggregate-fenced realization. `Rebuild` still needs
the existing aggregate fence and original source handle. Adoption carries the
same effective spec, handle and retained attempt in `SandboxAdoption`, plus the
explicit attached/unattached `SandboxEffect`. Existing backend methods retain
their current-authority, marker, projection, UID and resourceVersion checks.
Warm capacity can serve only unattached Create; fenced realization and Rebuild
continue through the exact underlying provider without a warm checkout.

Handle layout and closed-incarnation validation belong to the provider codec.
Session environment composition asks the one provider rather than interpreting
the same filesystem or container handle again. Filesystem absence may have no
incarnation; container absence, terminal and disposing evidence require the
original physical incarnation. These facts grant no disposal authority or
Session cleanup receipt. Restore capability is one `SandboxRestoration` value:
Unsupported, Targets, or Restorable. A Restorable rung always includes target
custody, so independent optional roles cannot advertise an inconsistent pair.

Kubernetes Create request/response wire values belong to the existing
`awaken-container-contract`. Moving those values changes their Rust import
path, not their serialized fields or the original ledger's identity and replay
law. Kubernetes package realization keeps one verifier, with bounded identity,
immutable projection and decoded-fact responsibilities in private modules;
retained-claim Pod cleanup remains the same durable interlock in a private
continuation module. Neither split introduces another scheduler, retry budget,
ledger, state machine, or physical cleanup authority.


The public Rust migration affects awaken-cloud's
`awaken-hosting-durable/src/container_continuation.rs`, its
`tests/provider_fixtures.rs`, and `restore_tests.rs`: replace the container-only
trait with `SandboxProvider<Environment = dyn SessionSandbox>`, implement its
`SandboxReadiness`, and supply the one `SandboxRestoration` rung. Vocabulary
uses `SandboxAdoptionLayout`, `SandboxAdoption`, `SandboxRealizationIntent` and
`SessionSandbox` from the provisioning contract. The Gateway's existing
allowlist capability and normalization imports do not change. This source
migration does not claim that awaken-cloud or a deployment has been qualified.
