# Wiki Update Log

## 2026-07-28

- **Update**: Amended ADR-0061 so embedded IAM and the side-effect-free
  `awaken management iam profile` release projection share one deterministic,
  Management-owned authorization contract.
- **Update**: Extracted the existing `awaken.runtime.resources` policy into the
  same authoritative path and exposed it through
  `awaken management iam profile resources`, so hosted File/Skill authorization
  cannot drift from embedded Management.

## 2026-07-26

- **Update**: Amended ADR-0067 with the neutral Native
  `PlatformProviderAdapter` realization: downstream platforms can bind an exact
  Platform trust-domain holder and claim-fenced receipt without adding cloud,
  gateway, IAM, route, or secret-store vocabulary to Awaken.

## 2026-07-25

- **Correction**: ADR-0066/0067 remain accepted target directions, but their
  feature slices are now gated by a contract-closure Slice 0. The correction
  freezes the ADR-0051 scope envelope, consumed preparation intent,
  claim-fenced Worker-to-Control application contribution, canonical Managed
  MCP full replacement, Session realization lease, replace/tombstone root CAS,
  sealed payload resolver, dispatch claim-epoch credential binding, exact OAuth
  refresh/reseal access, and planned-versus-actual realization split. Previous
  entries describing the targets as immediately development-ready are
  superseded.

## 2026-07-24

- **Update**: Accepted ADR-0067 as development-ready for Workload/Worker realization: CredentialAccess separates material source from recipient-bound envelope, Environment/attempt profile requests one exact allowed holder, MCP generation or RunAttempt pins it, and failure never changes holders. Automatic LLM Vault and Platform realization remain separately gated.

- **Update**: Accepted ADR-0066 as development-ready: one root mutation atomically owns immutable baseline, existing Resource state, MCP-only generations, idempotency, and outbox; Agent/Session/application MCP share one normalizer; exact generations stage invisibly, CAS Active, publish, drain, and recover. Generic Service/public realizer work remains deferred until old MCP authorities are deleted.

- **Update**: Amended ADR-0062 with the hosted configured-upstream composition,
  same-model complete-binding fallback semantics, and canonical Worker realization
  capability vocabulary.
- **Update**: Accepted ADR-0062: inference access is resolved once by Workspace at publication, fingerprinted in the executable snapshot, and only its published credential-injection contract reaches Runtime/Worker.

## 2026-07-20

- **Update**: Indexed ADR-0061 for selectable local identity, centralized authorization scope policy, and platform-managed resource ownership.

## 2026-06-27

- **Update**: Added explicit model-provider/model/model-pool/agent config graph guidance, including spec responsibilities, model binding selection, and model-pool fallback ownership.
- **Update**: Renamed the model-access provider record to `ModelProviderSpec` and clarified that `AgentSpec` must not absorb concrete launch, endpoint, or probe data.
- **Update**: Split immutable `RunActivation`, per-attempt `RuntimeRunContext`, and immutable `ExecutableAgentSnapshot` in runtime boundary and config-to-run guidance.
- **Update**: Clarified that live `StateStore` apply is not durable commit visibility, and reserved durable truth for `ThreadCommit` / `CommitCoordinator`.
- **Update**: Added neutral `ToolExecutor` guidance so host tools, MCP, remote, and client-executed tools remain execution-side adapters.
- **Update**: Clarified runtime axes and recorded that hook/tool/model calls cross execution, staging, commit, and projection before becoming durable truth.
- **Update**: Added facts for config-side publication coordination, registry compilation, and the runtime catalog install boundary.
- **Update**: Added executable snapshot contract facts and linked them to the config-to-run flow.
- **Update**: Added explicit boundary facts for protocol adapters, permission, binding, resources, errors, and package enforcement.
- **Update**: Added the config-to-run execution flow fact page and indexed it from [index.md](index.md).
- **Update**: Recorded the runtime configuration publication axis and neutral naming guardrail in retrieval facts.
- **Update**: Split durable wiki maintenance policy into [maintenance-notes.md](maintenance-notes.md), keeping `log.md` as OKF update history.

## 2026-06-26

- **Update**: Established this wiki as the retrieval index for the runtime design corpus.
- **Update**: Added the runtime protocol/license guardrail and related source-document ownership links.

## 2026-06-25

- **Initialization**: Created the source-document ownership index, fact indexes, agent instructions, and root navigation index.
- **Update**: Added retrieval facts for runtime behavior, runtime interface boundaries, tool and capability policy, deployment, resources, credentials, and product adapter boundaries.
