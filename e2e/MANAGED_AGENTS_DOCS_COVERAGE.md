# Managed Agents documentation traceability

This report maps the official Claude Managed Agents documentation inventory to
Awaken's implementation boundary and executable evidence. It is intentionally
not a test-design catalog:

- source code, contracts, and ADRs own behavior;
- cause/effect inventories, constraints, and decision rules live beside the
  corresponding Rust or TypeScript test;
- this file owns only page-level traceability and the local/external boundary.

The installed official `@anthropic-ai/sdk` version in `e2e/package.json` is the
wire oracle. `conformance/managed_docs_coverage_e2e.mjs` keeps this inventory
offline and deterministic by checking the exact page set, unique rows, named
executable evidence, and an explicit local/external classification.

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
| [dreams](https://platform.claude.com/docs/en/managed-agents/dreams) | Dream create, retrieve, list, cancel, archive, retention, and cleanup | `managed_dream_e2e.ts` | Local research-preview lifecycle ✅ |
| [environments](https://platform.claude.com/docs/en/managed-agents/environments) | Environment configuration, packages, network policy, lifecycle, and realization | `management_environments_e2e.mjs`, `managed_container_agent_e2e.mjs` | Docker/Podman local; unattested hosted controls external ◇ |
| [events-and-streaming](https://platform.claude.com/docs/en/managed-agents/events-and-streaming) | Durable send/list/stream, ordering, receipts, previews, reconnect, and interruption | `managed_processed_at_e2e.mjs`, `managed_live_previews_e2e.mjs` | Local list/SSE/replay contract ✅ |
| [files](https://platform.claude.com/docs/en/managed-agents/files) | Upload/download, Session mounts, paths, access mode, authorization, and release | `managed_resources_api_e2e.mjs`, `managed_content_blocks_e2e.ts` | Local resource and content-block boundary; hosted metadata external ◇ |
| [github](https://platform.claude.com/docs/en/managed-agents/github) | Repository realization, credential non-disclosure, rotation, and harvest | `managed_full_chain_e2e.mjs`, `secret_nonleak_e2e.mjs` | Local clone, mount, secret, and lifecycle behavior ✅ |
| [mcp-connector](https://platform.claude.com/docs/en/managed-agents/mcp-connector) | MCP references, URL and cardinality validation, tools, policy, retry, and errors | `managed_mcp_e2e.ts`, `management_mcp_e2e.mjs` | Local MCP contract and failure taxonomy ✅ |
| [mcp-tunnels](https://platform.claude.com/docs/en/managed-agents/mcp-tunnels) | Tunnel and certificate wire compatibility at the Awaken edge | `management_tunnels_contract_e2e.mjs` | Cloud custody and transport lifecycle remain external ◇ |
| [memory](https://platform.claude.com/docs/en/managed-agents/memory) | Memory beta, limits, mounts, CAS, history, redaction, and lifecycle | `management_memory_stores_e2e.mjs`, `managed_memory_extraction_stage_recovery_e2e.mjs` | Local API, persistence, and recovery behavior ✅ |
| [migration](https://platform.claude.com/docs/en/managed-agents/migration) | Mapping Messages/Agent SDK concepts to durable Agent, Session, Event, and tool contracts | `managed_full_lifecycle_e2e.mjs`, `managed_custom_e2e.mjs` | Local compatibility path; client migration planning external ◇ |
| [multiagent-orchestration](https://platform.claude.com/docs/en/managed-agents/multiagent-orchestration) | Frozen roster, child Threads, delegation, Advisor, follow-up, archive, and interrupt | `managed_delegation_e2e.mjs`, `delegated_remote_lifecycle_e2e.ts` | Local Native/ACP/A2A orchestration ✅ |
| [onboarding](https://platform.claude.com/docs/en/managed-agents/onboarding) | Agent, Environment, Session, and Event API journey | `managed_full_lifecycle_e2e.mjs` | API journey local; Console rendering and copy UX external ◇ |
| [overview](https://platform.claude.com/docs/en/managed-agents/overview) | Core resource graph, capabilities, history, steering, and scheduled execution | `managed_full_lifecycle_e2e.mjs`, `managed_capabilities_e2e.mjs` | Local critical-path behavior ✅ |
| [permission-policies](https://platform.claude.com/docs/en/managed-agents/permission-policies) | Frozen policy, requires-action blockers, allow, deny, and custom-tool separation | `managed_hitl_e2e.mjs`, `acp_permission_resume_e2e.mjs` | Local permission and exact-ticket behavior ✅ |
| [quickstart](https://platform.claude.com/docs/en/managed-agents/quickstart) | Official SDK Agent, Environment, Session, stream, tool, and idle journey | `managed_full_lifecycle_e2e.mjs` | Local official-SDK happy path ✅ |
| [reference](https://platform.claude.com/docs/en/managed-agents/reference) | Event catalogs, DTOs, worker flags, MCP transport, and rate limits | `gate_event_catalog_e2e.mjs`, `management_official_worker_e2e.mjs` | Local wire and worker contract; branding external ◇ |
| [scheduled-deployments](https://platform.claude.com/docs/en/managed-agents/scheduled-deployments) | Cron, timezone, previews, jitter, pause, archive, manual/scheduled runs, and recovery | `management_deployment_schedule_e2e.mjs` | Local durable scheduler behavior ✅ |
| [self-hosted-sandboxes](https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes) | Worker poll, claim, heartbeat, stop, reclaim, capacity, and custom tools | `management_self_hosted_worker_e2e.mjs`, `worker_transport_e2e.mjs` | Local Worker lifecycle ✅ |
| [self-hosted-sandboxes-security](https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes-security) | Credential, Workspace, network, filesystem, and Worker trust boundaries | `worker_resource_manifest_e2e.ts`, `secret_nonleak_e2e.mjs` | Local fail-closed boundary; operator hardening external ◇ |
| [session-operations](https://platform.claude.com/docs/en/managed-agents/session-operations) | Retrieve/list/filter/page, idle-only mutation, archive, delete, and cascade scope | `management_sessions_family_e2e.mjs`, `managed_session_pagination_e2e.mjs` | Local Session lifecycle and pagination ✅ |
| [sessions](https://platform.claude.com/docs/en/managed-agents/sessions) | Agent/environment binding, atomic initial Events, overrides, limits, and lazy execution | `management_sessions_family_e2e.mjs`, `managed_sdk_runtime_matrix_e2e.mjs` | Local Native/ACP creation and replay ✅ |
| [skills](https://platform.claude.com/docs/en/managed-agents/skills) | Skill beta, bundle/version pinning, attachment limits, realization, and release | `management_skills_e2e.mjs`, `managed_skill_bundle_pin_e2e.mjs` | Local bundle and frozen-version behavior ✅ |
| [tools](https://platform.claude.com/docs/en/managed-agents/tools) | Built-ins, WebSearch, custom tools, client results, policy, and output spill | `managed_capabilities_e2e.mjs`, `managed_custom_e2e.mjs` | Local tool-family and result-loop behavior ✅ |
| [vaults](https://platform.claude.com/docs/en/managed-agents/vaults) | Write-only secret kinds, limits, rotation, matching, lifecycle, and non-disclosure | `management_vaults_family_e2e.mjs`, `secret_nonleak_e2e.mjs` | Local secret-custody boundary ✅ |
| [webhooks](https://platform.claude.com/docs/en/managed-agents/webhooks) | Supported facts, signatures, SSRF fence, retry, dedupe, and disable policy | `webhook_plane_e2e.rs`, `crud.rs` | Local delivery and failure policy ✅ |

## Maintenance contract

- When official page membership changes, update this inventory and the offline
  inventory gate together.
- When behavior changes, update the authoritative code/ADR and the inline test
  design beside the executable test. Do not add a decision table here.
- A page row may name several suites, but it must not create a suite-per-page
  parallel path. Existing end-to-end journeys remain the executable owners.
- Current pass/fail counts and coverage percentages belong to CI artifacts, not
  this versioned traceability report.
