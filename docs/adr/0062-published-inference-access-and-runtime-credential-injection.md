# ADR-0062: Published inference access and runtime credential injection

- Status: Accepted
- Date: 2026-07-20
- Depends on: ADR-0031, ADR-0032, ADR-0043, ADR-0057, ADR-0061

## Context

Inference configuration had accumulated topology-shaped concepts and repeated
resolution. Admission, dispatch and worker startup could each observe a newer
model catalog or choose a different credential. A combined concrete adapter also
made a Worker appear to depend on the complete configuration catalog even after
the runtime port had become read-only.

Local inference and a gateway are not different domain modes. Both are an
endpoint, an adapter protocol, an upstream model name and a credential access
contract. Only endpoint and credential injection/usage differ.

## Decision

### Static view

```text
Configuration bounded context
  AgentDefinition -> AgentPublication -> ExecutableAgentSnapshot
       |                    |                       |
       |              resolve once by              +-- fingerprint includes
       |              Workspace scope                  InferenceAccess
       v                    v
  Model Catalog ----> CatalogInferenceAccessPublisher <---- Credential inventory

Runtime bounded context
  ExecutableAgentSnapshot -> CredentialInferenceMaterializer -> LlmExecutor
                                      |                              |
                                      +-- exact credential get       +-- endpoint call
                                      +-- version/owner/status check
                                      +-- permitted injection only

Forbidden runtime dependencies:
  Model Catalog, list credentials, default-model selection, route selection,
  configuration CRUD, Workspace constants
```

`InferenceAccess` is immutable, secret-free snapshot data. It contains the
published provider/route/endpoint pins, Workspace owner, and `CredentialAccess`.
`CredentialAccess` is the single description of credential revision, allowed
injection mechanisms and provider usage. Candidate failover composes complete
`InferenceAccess` values; it does not maintain a second parallel field set.

`ExecutableAgentSnapshot.metadata.inference_access` participates in snapshot
fingerprinting. The same agent configuration with a different route, credential
revision, injection policy or candidate set is therefore a different executable
snapshot.

The concrete adapters are intentionally separate:

- `CatalogInferenceAccessPublisher` belongs to configuration publication and
  depends on `CatalogRepo` plus credential inventory. It has no `SecretStore` or
  executor dependency.
- `CredentialInferenceMaterializer` belongs to execution composition and depends
  only on exact credential lookup, `SecretStore`, and an optional explicitly
  installed host executor. It cannot enumerate or select configuration.

An externally hosted or secretless Worker may supply another implementation of
the materialization port. That is an adapter choice, not an `ExecutorProvider`
domain abstraction and not a local/gateway branch.

### Dynamic view

```text
author       ConfigService       AccessPublisher       snapshot store
  | publish(scope, definition)          |                    |
  |------------------>|                 |                    |
  |                   | resolve(scope, ordered models)       |
  |                   |---------------->|                    |
  |                   |<----------------| pinned access      |
  |                   | fingerprint(spec + metadata/access)  |
  |                   |------------------------------------->|
  |<------------------| publication/version                 |

dispatcher       Worker/Host       CredentialMaterializer      provider
  | activation(snapshot) |                    |                    |
  |--------------------->|                    |                    |
  |                      | exact materialize(snapshot access)     |
  |                      |------------------->|                    |
  |                      |                    | get exact id       |
  |                      |                    | verify scope/revoked/version
  |                      |                    | inject as published|
  |                      |<-------------------| executor           |
  |                      |---------------------------------------->|
```

Catalog edits after publication cannot change an already dispatched run.
Revocation, owner mismatch, revision mismatch, unsupported injection kind, or a
model outside the pinned candidate set fails closed. Runtime never falls back to
a new global default or a weaker injection mechanism.

### Scope and authorization

Inference publication receives a trusted Workspace scope from the management
PEP. The publisher selects only inventory owned by that scope and records it in
the snapshot. Runtime validates the persisted credential owner against the pin;
it does not call IAM and does not infer scope from a resource id.

Awaken's authorization hierarchy stops at Org -> Workspace; Project is not a
Runtime scope. The single-machine composition hides Org and supplies its
persisted default Org/Workspace coordinates. Awaken Flow may add Project in its
own bounded context without changing this contract.

## Consequences

- Configuration has one resolution point and execution has one materialization
  point.
- Dispatch carries the snapshot; it has no independent model-access/admission
  truth.
- Local endpoints and gateways use the same data model and call path.
- Credential plaintext remains outside serializable snapshots and queue rows.
- Worker dependency graphs no longer include the model catalog merely to execute
  a published run.
- Safety depends on correct adapters, durable ownership facts and fail-closed
  composition; this architecture reduces attack surface but is not by itself a
  proof that every deployment is secure.

## Removed concepts

The implementation removes `RunDispatch.model_access`, its builder, runtime
`InferenceAccessResolver`/`ModelAccessResolver`, admission-time `pin_access`,
child-dispatch re-resolution, `ConfiguredInferenceMaterializer`, and its fixed
Workspace field. `credential_version` is replaced by the existing typed
`CredentialAccess.credential.revision`; duplicated candidate access fields are
replaced by composition of one `InferenceAccess` value.

No `InferencePlan`, local/gateway mode, or runtime `ExecutorProvider` is part of
the resulting domain language.

## Verification

- Rust tests cover scope selection, fingerprint changes, immutable route pins,
  revocation/version/owner rejection, injection-policy non-downgrade and pinned
  fallback candidates.
- `formal/tla/InferenceAccessPublication.tla` checks that access is published at
  most once, dispatch copies the published value, and runtime cannot create a
  different value.
- TypeScript E2E exercises the serialized snapshot through registered Worker
  claim/commit transport; anonymous claimed-commit remains an exact `401` check.
