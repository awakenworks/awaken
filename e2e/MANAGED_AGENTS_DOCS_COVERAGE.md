# Managed Agents documentation traceability

This report maps both the official Claude Managed Agents documentation inventory
and every Managed method exposed by the pinned official TypeScript SDK to
Awaken's implementation boundary and executable evidence. It is intentionally
not a test-design catalog:

- source code, contracts, and ADRs own behavior;
- cause/effect inventories, constraints, and decision rules live beside the
  corresponding Rust or TypeScript test;
- this file owns only page/method traceability and the local/external boundary.

The installed official `@anthropic-ai/sdk` version in `e2e/package.json` is the
wire oracle. `conformance/managed_ts_sdk_method_manifest.mjs` is the sole
method-level ownership inventory; the appendix below is only its checked
Markdown projection. `conformance/managed_docs_coverage_e2e.mjs` keeps both
inventories offline and deterministic by checking the exact page set, unique
rows, every SDK method, named executable evidence, and explicit coverage status.
Its opt-in live mode compares that same page snapshot with the official
[`docs/llms.txt`](https://platform.claude.com/docs/llms.txt) index without
persisting a second inventory.

The method scope is the SDK's Managed `client.beta` surface excluding the
general Messages API, plus the GA Models, Files, and Skills companions used by
Managed Agents. Message Batches, token counting, and other general Claude API
families are intentionally outside this report rather than counted as gaps.

`✅` means the Awaken-owned boundary has local executable evidence. `◇` means
the page also covers Anthropic-hosted infrastructure, Console/SDK UX, branding,
or operator controls which a self-hosted implementation cannot truthfully
execute; the Awaken-owned API/security boundary still has executable evidence.

## Official page inventory

| Official page | Awaken-owned contract and boundary | Executable evidence | Coverage boundary |
|---|---|---|---|
| [agent-setup](https://platform.claude.com/docs/en/managed-agents/agent-setup) | Agent defaults, versioned updates, frozen rosters, archive terminality | `management_agents_e2e.mjs`, `agent_publication_truth_e2e.ts` | Local state, CAS, and frozen-version behavior ✅ |
| [budgets](https://platform.claude.com/docs/en/managed-agents/budgets) | Frozen list-price snapshot, cumulative usage, hard ceiling, raise and removal | `management_sessions_family_e2e.mjs` | Local pricing snapshot and lifecycle projection ✅ |
| [cloud-sandboxes-reference](https://platform.claude.com/docs/en/managed-agents/cloud-sandboxes-reference) | Configuration admission and fail-closed sandbox provisioning | `managed_container_agent_e2e.mjs`, `sandbox_provisioning_e2e.mjs` | Hosted image and machine inventory remain external ◇ |
| [define-outcomes](https://platform.claude.com/docs/en/managed-agents/define-outcomes) | Rubric admission, evaluation lifecycle, recovery, and terminal results | `managed_outcome_recovery_e2e.ts`, `managed_outcome_lifecycle_e2e.mjs` | Local state machine and interruption behavior ✅ |
| [dreams](https://platform.claude.com/docs/en/managed-agents/dreams) | Dream create, retrieve, list, running output transition, cancel with trailing usage, archive, timeout/input-size/org-limit errors, retention, and cleanup | `managed_dream_e2e.ts`, `dreams.rs`, `awaken-dream-application`, `awaken-coordinator::dream` | Local protocol/state/error lifecycle; hosted synthesis quality, billing, and service limits external ◇ |
| [environments](https://platform.claude.com/docs/en/managed-agents/environments) | Environment configuration, packages, network policy, lifecycle, and realization | `management_environments_e2e.mjs`, `managed_container_agent_e2e.mjs` | Docker/Podman local; unattested hosted controls external ◇ |
| [events-and-streaming](https://platform.claude.com/docs/en/managed-agents/events-and-streaming) | Durable send/list/stream, ordering, receipts, previews, reconnect, and interruption | `managed_processed_at_e2e.mjs`, `managed_live_previews_e2e.mjs` | Local list/SSE/replay contract ✅ |
| [files](https://platform.claude.com/docs/en/managed-agents/files) | Upload/download, Session mounts, paths, access mode, authorization, and release | `managed_resources_api_e2e.mjs`, `managed_content_blocks_e2e.ts` | Local resource and content-block boundary; hosted metadata external ◇ |
| [github](https://platform.claude.com/docs/en/managed-agents/github) | Repository realization, credential non-disclosure, rotation, and harvest | `managed_full_chain_e2e.mjs`, `secret_nonleak_e2e.mjs` | Local clone, mount, secret, and lifecycle behavior ✅ |
| [mcp-connector](https://platform.claude.com/docs/en/managed-agents/mcp-connector) | MCP references, URL and cardinality validation, tools, policy, retry, and errors | `managed_mcp_e2e.ts`, `management_mcp_e2e.mjs` | Local MCP contract and failure taxonomy ✅ |
| [memory](https://platform.claude.com/docs/en/managed-agents/memory) | Memory beta, limits, mounts, CAS, history, redaction, and lifecycle | `management_memory_stores_e2e.mjs`, `managed_memory_extraction_stage_recovery_e2e.mjs` | Local API, persistence, and recovery behavior ✅ |
| [migration](https://platform.claude.com/docs/en/managed-agents/migration) | Mapping Messages/Agent SDK concepts to durable Agent, Session, Event, and tool contracts | `managed_full_lifecycle_e2e.mjs`, `managed_custom_e2e.mjs` | Local compatibility path; client migration planning external ◇ |
| [multiagent-orchestration](https://platform.claude.com/docs/en/managed-agents/multiagent-orchestration) | Frozen roster, child Threads, delegation, Advisor, follow-up, archive, and interrupt | `managed_delegation_e2e.mjs`, `delegated_remote_lifecycle_e2e.ts` | Local Native/ACP/A2A orchestration ✅ |
| [onboarding](https://platform.claude.com/docs/en/managed-agents/onboarding) | Agent, Environment, Session, and Event API journey | `managed_full_lifecycle_e2e.mjs` | API journey local; Console rendering and copy UX external ◇ |
| [overview](https://platform.claude.com/docs/en/managed-agents/overview) | Core resource graph, capabilities, history, steering, and scheduled execution | `managed_full_lifecycle_e2e.mjs`, `managed_capabilities_e2e.mjs` | Local critical-path behavior ✅ |
| [permission-policies](https://platform.claude.com/docs/en/managed-agents/permission-policies) | Frozen policy, requires-action blockers, allow, deny, and custom-tool separation | `managed_hitl_e2e.mjs`, `acp_permission_resume_e2e.mjs` | Local permission and exact-ticket behavior ✅ |
| [quickstart](https://platform.claude.com/docs/en/managed-agents/quickstart) | Official SDK Agent, Environment, Session, stream, tool, and idle journey | `managed_full_lifecycle_e2e.mjs` | Local official-SDK happy path ✅ |
| [reference](https://platform.claude.com/docs/en/managed-agents/reference) | Event catalogs, DTOs, worker flags, MCP transport, and rate limits | `gate_event_catalog_e2e.mjs`, `management_official_worker_e2e.mjs` | Local wire and worker contract; branding external ◇ |
| [scheduled-deployments](https://platform.claude.com/docs/en/managed-agents/scheduled-deployments) | Cron/DST, previews, jitter, per-Session budgets, paused manual run, outcome seed, rate-limit/subagent failures, webhooks, and durable occurrence claims | `management_deployments_e2e.mjs`, `management_deployment_schedule_e2e.mjs`, `managed_deployment_e2e.rs`, `webhook_plane_e2e.rs` | Local durable scheduler and official SDK behavior ✅ |
| [self-hosted-sandboxes](https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes) | Worker poll, claim, heartbeat, stop, reclaim, capacity, and custom tools | `management_self_hosted_worker_e2e.mjs`, `worker_transport_e2e.mjs` | Local Worker lifecycle ✅ |
| [self-hosted-sandboxes-security](https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes-security) | Credential, Workspace, network, filesystem, and Worker trust boundaries | `worker_resource_manifest_e2e.ts`, `secret_nonleak_e2e.mjs` | Local fail-closed boundary; operator hardening external ◇ |
| [session-operations](https://platform.claude.com/docs/en/managed-agents/session-operations) | Retrieve/list/filter/page, idle-only mutation, archive, delete, and cascade scope | `management_sessions_family_e2e.mjs`, `managed_session_pagination_e2e.mjs` | Local Session lifecycle and pagination ✅ |
| [sessions](https://platform.claude.com/docs/en/managed-agents/sessions) | Agent/environment binding, atomic initial Events, overrides, limits, and lazy execution | `management_sessions_family_e2e.mjs`, `managed_sdk_runtime_matrix_e2e.mjs` | Local Native/ACP creation and replay ✅ |
| [skills](https://platform.claude.com/docs/en/managed-agents/skills) | Skill beta, bundle/version pinning, attachment limits, realization, and release | `management_skills_e2e.mjs`, `managed_skill_bundle_pin_e2e.mjs` | Local bundle and frozen-version behavior ✅ |
| [tools](https://platform.claude.com/docs/en/managed-agents/tools) | Built-ins, WebSearch, custom tools, client results, policy, and output spill | `managed_capabilities_e2e.mjs`, `managed_custom_e2e.mjs` | Local tool-family and result-loop behavior ✅ |
| [vaults](https://platform.claude.com/docs/en/managed-agents/vaults) | Write-only secret kinds, limits, rotation, matching, lifecycle, and non-disclosure | `management_vaults_family_e2e.mjs`, `secret_nonleak_e2e.mjs` | Local secret-custody boundary ✅ |
| [webhooks](https://platform.claude.com/docs/en/managed-agents/webhooks) | Supported Session/Deployment facts, signatures, official SDK unwrap, SSRF fence, retry, dedupe, and disable policy | `webhook_plane_e2e.rs`, `managed_webhooks_official_sdk_e2e.mjs`, `crud.rs` | Local delivery and failure policy ✅ |

## Official TypeScript SDK method coverage

The table is an exact projection of the installed SDK's 131 Managed methods.
“Covered” means the named deterministic scenario invokes that official SDK method
directly, or invokes the one explicitly named SDK helper. The adjacent manifest gate
also rejects missing SDK methods, duplicate ownership, owners outside the deterministic
execution graph, and owners that do not call the declared method.

<!-- managed-sdk-method-coverage:start -->
| Official TypeScript SDK method | Route family | Coverage | Executable E2E scenario |
|---|---|---|---|
| `beta.agents.archive` | `/v1/agents/{}/archive` | ✅ covered | `management_agents_e2e.mjs` |
| `beta.agents.create` | `/v1/agents` | ✅ covered | `management_agents_e2e.mjs` |
| `beta.agents.list` | `/v1/agents` | ✅ covered | `management_agents_e2e.mjs` |
| `beta.agents.retrieve` | `/v1/agents/{}` | ✅ covered | `management_agents_e2e.mjs` |
| `beta.agents.update` | `/v1/agents/{}` | ✅ covered | `management_agents_e2e.mjs` |
| `beta.agents.versions.list` | `/v1/agents/{}/versions` | ✅ covered | `management_agents_e2e.mjs` |
| `beta.deploymentRuns.list` | `/v1/deployment_runs` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deploymentRuns.retrieve` | `/v1/deployment_runs/{}` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.archive` | `/v1/deployments/{}/archive` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.create` | `/v1/deployments` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.list` | `/v1/deployments` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.pause` | `/v1/deployments/{}/pause` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.retrieve` | `/v1/deployments/{}` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.run` | `/v1/deployments/{}/run` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.unpause` | `/v1/deployments/{}/unpause` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.deployments.update` | `/v1/deployments/{}` | ✅ covered | `management_deployments_e2e.mjs` |
| `beta.dreams.archive` | `/v1/dreams/{}/archive` | ✅ covered | `managed_dream_e2e.ts` |
| `beta.dreams.cancel` | `/v1/dreams/{}/cancel` | ✅ covered | `managed_dream_e2e.ts` |
| `beta.dreams.create` | `/v1/dreams` | ✅ covered | `managed_dream_e2e.ts` |
| `beta.dreams.list` | `/v1/dreams` | ✅ covered | `managed_dream_e2e.ts` |
| `beta.dreams.retrieve` | `/v1/dreams/{}` | ✅ covered | `managed_dream_e2e.ts` |
| `beta.environments.archive` | `/v1/environments/{}/archive` | ✅ covered | `management_environments_e2e.mjs` |
| `beta.environments.create` | `/v1/environments` | ✅ covered | `management_environments_e2e.mjs` |
| `beta.environments.delete` | `/v1/environments/{}` | ✅ covered | `management_environments_e2e.mjs` |
| `beta.environments.list` | `/v1/environments` | ✅ covered | `management_environments_e2e.mjs` |
| `beta.environments.retrieve` | `/v1/environments/{}` | ✅ covered | `management_environments_e2e.mjs` |
| `beta.environments.update` | `/v1/environments/{}` | ✅ covered | `management_environments_e2e.mjs` |
| `beta.environments.work.ack` | `/v1/environments/{}/work/{}/ack` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.heartbeat` | `/v1/environments/{}/work/{}/heartbeat` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.list` | `/v1/environments/{}/work` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.poll` | `/v1/environments/{}/work/poll` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.poller` | `generated-worker-helper` | ✅ covered | `management_official_worker_e2e.mjs` |
| `beta.environments.work.retrieve` | `/v1/environments/{}/work/{}` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.stats` | `/v1/environments/{}/work/stats` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.stop` | `/v1/environments/{}/work/{}/stop` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.update` | `/v1/environments/{}/work/{}` | ✅ covered | `management_environment_work_depth_e2e.mjs` |
| `beta.environments.work.worker` | `generated-worker-helper` | ✅ covered | `management_official_worker_e2e.mjs` |
| `beta.files.delete` | `/v1/files/{}` | ✅ covered | `management_files_models_e2e.mjs` |
| `beta.files.download` | `/v1/files/{}/content` | ✅ covered | `managed_dream_e2e.ts` |
| `beta.files.list` | `/v1/files` | ✅ covered | `managed_resources_api_e2e.mjs` |
| `beta.files.retrieveMetadata` | `/v1/files/{}` | ✅ covered | `managed_full_lifecycle_e2e.mjs` |
| `beta.files.upload` | `/v1/files` | ✅ covered | `management_files_models_e2e.mjs` |
| `beta.memoryStores.archive` | `/v1/memory_stores/{}/archive` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.create` | `/v1/memory_stores` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.delete` | `/v1/memory_stores/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.list` | `/v1/memory_stores` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memories.create` | `/v1/memory_stores/{}/memories` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memories.delete` | `/v1/memory_stores/{}/memories/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memories.list` | `/v1/memory_stores/{}/memories` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memories.retrieve` | `/v1/memory_stores/{}/memories/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memories.update` | `/v1/memory_stores/{}/memories/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memoryVersions.list` | `/v1/memory_stores/{}/memory_versions` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memoryVersions.redact` | `/v1/memory_stores/{}/memory_versions/{}/redact` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.memoryVersions.retrieve` | `/v1/memory_stores/{}/memory_versions/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.retrieve` | `/v1/memory_stores/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.memoryStores.update` | `/v1/memory_stores/{}` | ✅ covered | `management_memory_stores_e2e.mjs` |
| `beta.models.list` | `/v1/models` | ✅ covered | `management_files_models_e2e.mjs` |
| `beta.models.retrieve` | `/v1/models/{}` | ✅ covered | `management_files_models_e2e.mjs` |
| `beta.sessions.archive` | `/v1/sessions/{}/archive` | ✅ covered | `management_sessions_family_e2e.mjs` |
| `beta.sessions.create` | `/v1/sessions` | ✅ covered | `management_sessions_family_e2e.mjs` |
| `beta.sessions.delete` | `/v1/sessions/{}` | ✅ covered | `management_sessions_family_e2e.mjs` |
| `beta.sessions.events.list` | `/v1/sessions/{}/events` | ✅ covered | `managed_e2e.mjs` via `harness.mjs#waitForSessionEventReceipt` |
| `beta.sessions.events.send` | `/v1/sessions/{}/events` | ✅ covered | `managed_e2e.mjs` |
| `beta.sessions.events.stream` | `/v1/sessions/{}/events/stream` | ✅ covered | `managed_e2e.mjs` |
| `beta.sessions.events.toolRunner` | `generated-tool-runner-helper` | ✅ covered | `managed_session_tool_runner_matrix_e2e.mjs` |
| `beta.sessions.list` | `/v1/sessions` | ✅ covered | `management_sessions_family_e2e.mjs` |
| `beta.sessions.resources.add` | `/v1/sessions/{}/resources` | ✅ covered | `managed_resource_lifecycle_e2e.mjs` |
| `beta.sessions.resources.delete` | `/v1/sessions/{}/resources/{}` | ✅ covered | `managed_resource_lifecycle_e2e.mjs` |
| `beta.sessions.resources.list` | `/v1/sessions/{}/resources` | ✅ covered | `managed_resource_lifecycle_e2e.mjs` |
| `beta.sessions.resources.retrieve` | `/v1/sessions/{}/resources/{}` | ✅ covered | `managed_resource_lifecycle_e2e.mjs` |
| `beta.sessions.resources.update` | `/v1/sessions/{}/resources/{}` | ✅ covered | `managed_session_resource_rotation_e2e.mjs` |
| `beta.sessions.retrieve` | `/v1/sessions/{}` | ✅ covered | `management_sessions_family_e2e.mjs` |
| `beta.sessions.threads.archive` | `/v1/sessions/{}/threads/{}/archive` | ✅ covered | `management_session_threads_e2e.mjs` |
| `beta.sessions.threads.events.list` | `/v1/sessions/{}/threads/{}/events` | ✅ covered | `management_session_threads_e2e.mjs` |
| `beta.sessions.threads.events.stream` | `/v1/sessions/{}/threads/{}/stream` | ✅ covered | `management_session_threads_e2e.mjs` |
| `beta.sessions.threads.list` | `/v1/sessions/{}/threads` | ✅ covered | `management_session_threads_e2e.mjs` |
| `beta.sessions.threads.retrieve` | `/v1/sessions/{}/threads/{}` | ✅ covered | `management_session_threads_e2e.mjs` |
| `beta.sessions.update` | `/v1/sessions/{}` | ✅ covered | `management_sessions_family_e2e.mjs` |
| `beta.skills.create` | `/v1/skills` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.delete` | `/v1/skills/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.list` | `/v1/skills` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.retrieve` | `/v1/skills/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.versions.create` | `/v1/skills/{}/versions` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.versions.delete` | `/v1/skills/{}/versions/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.versions.download` | `/v1/skills/{}/versions/{}/content` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.versions.list` | `/v1/skills/{}/versions` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.skills.versions.retrieve` | `/v1/skills/{}/versions/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `beta.tunnels.archive` | `/v1/tunnels/{}/archive` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.certificates.archive` | `/v1/tunnels/{}/certificates/{}/archive` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.certificates.create` | `/v1/tunnels/{}/certificates` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.certificates.list` | `/v1/tunnels/{}/certificates` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.certificates.retrieve` | `/v1/tunnels/{}/certificates/{}` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.create` | `/v1/tunnels` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.list` | `/v1/tunnels` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.retrieve` | `/v1/tunnels/{}` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.revealToken` | `/v1/tunnels/{}/reveal_token` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.tunnels.rotateToken` | `/v1/tunnels/{}/rotate_token` | ✅ covered | `management_tunnels_contract_e2e.mjs` |
| `beta.userProfiles.create` | `/v1/user_profiles` | ✅ covered | `management_user_profiles_e2e.mjs` |
| `beta.userProfiles.createEnrollmentURL` | `/v1/user_profiles/{}/enrollment_url` | ✅ covered | `management_user_profiles_e2e.mjs` |
| `beta.userProfiles.list` | `/v1/user_profiles` | ✅ covered | `management_user_profiles_e2e.mjs` |
| `beta.userProfiles.retrieve` | `/v1/user_profiles/{}` | ✅ covered | `management_user_profiles_e2e.mjs` |
| `beta.userProfiles.update` | `/v1/user_profiles/{}` | ✅ covered | `management_user_profiles_e2e.mjs` |
| `beta.vaults.archive` | `/v1/vaults/{}/archive` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.create` | `/v1/vaults` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.credentials.archive` | `/v1/vaults/{}/credentials/{}/archive` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.credentials.create` | `/v1/vaults/{}/credentials` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.credentials.delete` | `/v1/vaults/{}/credentials/{}` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.credentials.list` | `/v1/vaults/{}/credentials` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.credentials.mcpOAuthValidate` | `/v1/vaults/{}/credentials/{}/mcp_oauth_validate` | ✅ covered | `management_vaults_e2e.mjs` |
| `beta.vaults.credentials.retrieve` | `/v1/vaults/{}/credentials/{}` | ✅ covered | `management_vaults_e2e.mjs` |
| `beta.vaults.credentials.update` | `/v1/vaults/{}/credentials/{}` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.delete` | `/v1/vaults/{}` | ✅ covered | `management_vaults_e2e.mjs` |
| `beta.vaults.list` | `/v1/vaults` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.vaults.retrieve` | `/v1/vaults/{}` | ✅ covered | `management_vaults_e2e.mjs` |
| `beta.vaults.update` | `/v1/vaults/{}` | ✅ covered | `management_vaults_family_e2e.mjs` |
| `beta.webhooks.unwrap` | `offline-standard-webhooks` | ✅ covered | `managed_webhooks_official_sdk_e2e.mjs` via `conformance/official_webhook_contract.mjs#exerciseOfficialWebhookContract` |
| `files.delete` | `/v1/files/{}` | ✅ covered | `management_files_models_e2e.mjs` |
| `files.download` | `/v1/files/{}/content` | ✅ covered | `managed_dream_e2e.ts` |
| `files.list` | `/v1/files` | ✅ covered | `management_files_models_e2e.mjs` |
| `files.retrieveMetadata` | `/v1/files/{}` | ✅ covered | `management_files_models_e2e.mjs` |
| `files.upload` | `/v1/files` | ✅ covered | `management_files_models_e2e.mjs` |
| `models.list` | `/v1/models` | ✅ covered | `management_files_models_e2e.mjs` |
| `models.retrieve` | `/v1/models/{}` | ✅ covered | `management_files_models_e2e.mjs` |
| `skills.create` | `/v1/skills` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.delete` | `/v1/skills/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.list` | `/v1/skills` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.retrieve` | `/v1/skills/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.versions.create` | `/v1/skills/{}/versions` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.versions.delete` | `/v1/skills/{}/versions/{}` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.versions.list` | `/v1/skills/{}/versions` | ✅ covered | `management_skills_e2e.mjs` |
| `skills.versions.retrieve` | `/v1/skills/{}/versions/{}` | ✅ covered | `management_skills_e2e.mjs` |
<!-- managed-sdk-method-coverage:end -->


## Maintenance contract

- When official page membership changes, update this inventory and the offline
  inventory gate together; use `npm run test:docs-coverage:live` to detect live
  sitemap drift.
- When the pinned SDK changes, update the sole method manifest and regenerate
  this checked appendix; the method-inventory gate rejects missing or duplicate
  methods and evidence owners.
- When behavior changes, update the authoritative code/ADR and the inline test
  design beside the executable test. Do not add a decision table here.
- A page row may name several suites, but it must not create a suite-per-page
  parallel path. Existing end-to-end journeys remain the executable owners.
- Current pass/fail counts and coverage percentages belong to CI artifacts, not
  this versioned traceability report.
