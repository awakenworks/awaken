# ADR-0062: Published model candidates and runtime credential injection

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
       |              Workspace scope                  complete model candidates
       v                    v
  Model Catalog ----> CatalogModelPublicationResolver <---- Credential inventory

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

`ResolvedModelCandidate` is the immutable, secret-free published model value. Its
`ModelProvisioning::Provider` variant contains the provider/route/endpoint pins,
Workspace owner, and existing `CredentialAccess`; `HostExecutor` names an explicit
host-installed executor. The primary and fallback candidates live only in
`ResolvedSpec.model_binding`/`model_candidates`, so the complete execution choice
is already covered by the snapshot fingerprint. There is no parallel
`InferenceAccess` metadata projection.

The concrete adapters are intentionally separate:

- `CatalogModelPublicationResolver` belongs to configuration publication and
  depends on `CatalogRepo` plus credential inventory. It has no `SecretStore` or
  executor dependency.
- `CredentialInferenceMaterializer` belongs to execution composition and depends
  only on exact credential lookup, `SecretStore`, and an optional explicitly
  installed host executor. It cannot enumerate or select configuration.
- `PinnedCredentialMaterializer` is the shared worker/host adapter used by both
  native provider execution and ACP provisioning. It verifies the exact published
  Workspace/revision/provider/usage pin before opening persisted material. ACP CLI
  capability discovery does not grant a provider route.

Process environment is not an inference configuration source. Provider endpoints,
models, fallback candidates and credential material must be authored through the
management UI/API and persisted before publication. An admin-only discovery query
may return a secret-free proposal (variable name/presence and non-secret coordinates),
but a proposal is neither a catalog row nor executable access and is never auto-applied.
Legacy `CredentialKind::Env` rows remain decodable for migration visibility but reject
creation and materialization.

An externally hosted or secretless Worker may supply another implementation of
the materialization port. That is an adapter choice, not an inference-executor
selection domain abstraction and not a local/gateway branch. The independently
named `ToolExecutorProvider` remains the placement port for remote tool execution;
it does not participate in model access or credential handling.

### Dynamic view

```text
author       ConfigService       ModelResolver         snapshot store
  | publish(scope, definition)          |                    |
  |------------------>|                 |                    |
  |                   | resolve(scope, ordered models)       |
  |                   |---------------->|                    |
  |                   |<----------------| complete candidates|
  |                   | fingerprint(resolved spec)           |
  |                   |------------------------------------->|
  |<------------------| publication/version                 |

dispatcher       Worker/Host       CredentialMaterializer      provider
  | activation(snapshot) |                    |                    |
  |--------------------->|                    |                    |
  |                      | exact materialize(snapshot candidate)  |
  |                      |------------------->|                    |
  |                      |                    | get exact id       |
  |                      |                    | verify scope/revoked/version
  |                      |                    | inject as published|
  |                      |<-------------------| executor           |
  |                      |---------------------------------------->|
```

Catalog edits or process-environment changes after publication cannot change an already dispatched run.
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
- Database-backed Catalog/Credential publication is the only provider execution
  truth; environment discovery cannot skip authoring or publication.
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
`CredentialAccess.credential.revision`; the duplicate `InferenceAccess` type and
`ExecutableAgentSnapshot.metadata.inference_access` projection are deleted because
`ResolvedModelCandidate::provisioning` already contains the complete route and
credential contract. The unused
`CredentialInjectionPolicy`, `CredentialPolicyError`, and `InjectedCredential`
types are deleted: execution receives one selected `CredentialInjectionKind`,
not a fallback list it could reinterpret.

The later single-truth-source cleanup also removes runtime
`AWAKEN_MODEL_FALLBACKS`, production `AWAKEN_ACP_ARGV`, ambient ACP provider/gateway
resolution and host native-credential-file projection. Candidate failover remains a
published `ResolvedModelCandidate` set; fixed launch sources remain explicit
dev/test composition only.

The same cleanup also removes the two resource-layer `ResourceWorkspace`
projection structs and their conversion middleware; resource adapters consume
the shared `awaken_tenancy::WorkspaceScope` coordinate directly. The public
unresolved `compile`/`compile_with_resource_prompts` entry points are removed in
favor of `compile_resolved`, so production publication cannot bypass scoped
access resolution. Dead MCP configuration vocabulary (`TransportTypeId`,
`RestartPolicy`, `McpServerConnectionConfig`, and the `awaken-ext-mcp` config
module), the duplicate runtime capability projection, the unused
`run_with_inference_materializer_and_upstream` worker overload, and the unused
`ResourceOwners::new`/`Default` construction path are removed as well.

No `InferencePlan`, local/gateway mode, or runtime inference-executor provider is
part of the resulting domain language.

## Verification

- Rust tests cover scope selection, fingerprint changes, immutable route pins,
  revocation/version/owner rejection, injection-policy non-downgrade and pinned
  fallback candidates.
- `formal/tla/InferenceAccessPublication.tla` checks that access is published at
  most once, dispatch copies the published value, later catalog edits cannot
  redirect it, and credential revocation/revision changes reject rather than
  materialize a stale pin.
- TypeScript E2E exercises the serialized snapshot through registered Worker
  claim/commit transport; anonymous claimed-commit remains an exact `401` check.
