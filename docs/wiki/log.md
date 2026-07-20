# Wiki Update Log

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
