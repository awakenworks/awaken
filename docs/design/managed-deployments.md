# Managed Deployments And Scheduling

## End-to-end objective

Managed Deployment is the durable control-plane resource that binds a concrete
Agent version, Environment, initial Events, Resources, Vaults, and an optional
POSIX cron schedule. Each trigger creates one append-only DeploymentRun and, on
successful admission, one ordinary Managed Session. Deployment never owns
Session execution or history.

```text
Deployment API -> DeploymentState -> DeploymentRepository
                         |                    |
                         |                    `- aggregate/run rows
                         |                    `- exact occurrence claim
                         |                    `- ManagedLifecycleFact outbox
                         v
                DeploymentSessionLauncher
                    |- local adapter (AllInOne)
                    `- Coordinator client -> launch handler
                                           -> canonical Session create command
```

The production periodic driver evaluates both cron Deployments and opt-in Dream
policies. It is one timer, not one combined aggregate: cron submits an ordinary
Session, while a Dream policy submits the same Dream job used by `POST /v1/dreams`.

## Role Catalog

The verified duplication review classifies every relevant owner as reused,
modified, or genuinely new before describing the dependency graph.

### Reused unchanged

| Authority | Owner | Role |
|---|---|---|
| POSIX cron and IANA timezone calculation | `awaken-protocol-managed::cron` | exact wall-clock occurrences, including DST behavior |
| Agent authoring/version truth | `ManagedAgentRepository` | validates and freezes the requested latest or pinned Agent version |
| Session creation and initial Event admission | `ManagedState::create_session_with_initial_events` | the only Deployment-to-execution path |
| organization create admission | `ManagedRateLimiter` | shares the ordinary Session-create bucket |
| webhook delivery and retry | `WebhookLifecycleSink` | drains the sole Managed lifecycle outbox |

### Modified existing owners

| Owner | Change | Result |
|---|---|---|
| `DeploymentState` | repository restore, Workspace checks, persistent cursor, exact occurrence claim, Agent version resolution | restart-safe and replica-safe aggregate projection |
| Managed Session store | Deployment, DeploymentRun, occurrence-claim migrations and SQLite/Postgres adapters | business row and lifecycle fact commit atomically |
| control/server composition | shares the Agent repository and binds the ordinary Session launcher | no duplicate Agent lookup or execution path |
| `DeploymentSessionLauncher` request | add the existing stable `deployment_run_id` | remote retries resolve to at most one Session |
| Agent archive operation | cascades terminal archive to live primary-Agent Deployments | no later scheduled run |
| Managed periodic driver | evaluates Deployment then Dream policies every 15 seconds | one production timer |

### Genuinely new

| Component | Owner | Responsibility |
|---|---|---|
| `DeploymentRepository` | `awaken-deployment-contract` | opaque durable records plus atomic scheduled-occurrence claim |
| `DeploymentRecord` / `DeploymentRunRecord` store adapters | `awaken-session-store` | SQLite/Postgres persistence without protocol DTO dependency |
| `CoordinatorDeploymentSessionClient` | Deployment adapter | invoke the same Session launch port across a process boundary |
| `DeploymentSessionLaunchHandler` | Coordinator adapter | idempotently lower one DeploymentRun into the canonical Session command |

There is no second cron parser, Session launcher, Agent registry, Deployment
cache authority, webhook outbox, scheduler loop, or remote-only Session domain
model. The local and remote launch adapters implement the same port.

## Static structure and contracts

```text
HTTP adapter (official DTOs)
  `- DeploymentState (application aggregate)
       |- ManagedAgentRepository (latest/pinned version resolution)
       |- DeploymentRepository (durability + occurrence claim + lifecycle fact)
       |- ManagedRateLimiter (create admission)
       `- DeploymentSessionLauncher
            |- LocalManagedDeploymentSessionLauncher (AllInOne)
            `- CoordinatorDeploymentSessionClient
                 `- DeploymentSessionLaunchHandler
                      `- ManagedState (ordinary Session/Event authority)

ManagedLifecycleFact outbox
  `- WebhookLifecycleSink -> official deployment.* / deployment_run.* events
```

`DeploymentState` keeps a locked working projection for fast list/retrieve and
schedule evaluation. The repository remains the restart and multi-replica
authority. Stored payloads are opaque JSON at the port, so storage adapters do
not depend on Managed wire DTOs. `claim_id` is the stable pair
`(deployment_id, scheduled_at)`; a unique row makes exactly one replica the
winner.

The launcher input includes `deployment_run_id`; this is an existing durable
business identity, not a new aggregate. The Coordinator handler records or reads
the one Session associated with that identity before returning. A retry after an
ambiguous transport failure returns the original `session_id`.

The Agent input boundary accepts a bare id or `{type:"agent", id, version?}`.
A bare id or omitted version resolves through the same Agent repository used by
`/v1/agents`, and the resulting concrete version is stored. Deployment rejects
override objects, unknown Agents, and disabled/archived Agents before mutation.

## Dynamic behavior

### Create and update

```text
request + Workspace scope
  -> decode official union and validate cron/timezone/bounds
  -> resolve current or pinned published Agent version
  -> enforce 1,000 scheduled-Deployment organization limit
  -> atomically persist Deployment + deployment.created|updated fact
  -> publish the committed projection
```

Archive is idempotent and terminal. Pause suppresses only future scheduled
triggers; manual run remains allowed. Unpause sets the cursor to the first future
occurrence and never backfills missed occurrences.

### Manual run

```text
POST .../run
  -> reject only missing/archived Deployment
  -> persist DeploymentRun with stable deployment_run_id
  -> call DeploymentSessionLauncher with deployment_run_id
  -> local adapter or Coordinator client reaches the same Session command
  -> create or return the one Session for deployment_run_id
  -> persist succeeded(session_id) or failed(error) + lifecycle fact
  -> return the terminal DeploymentRun projection
```

Initial Events are never sent through a second best-effort call after Session
creation. A request-level Session creation failure is DeploymentRun truth;
subsequent Session execution remains Session truth.

Transport unavailability before a conclusive launch outcome is retryable. It
must not be persisted as a permanent business failure until the Coordinator can
prove that no Session was created. This distinction prevents an ambiguous
network timeout from becoming a duplicate Session or false terminal outcome.

### Scheduled occurrence and replica claim

```text
timer tick
  -> find active, non-archived due cron occurrences
  -> retain exact scheduled_at; apply stable execution jitter (0s..10s)
  -> if primary Agent is missing/archived: archive Deployment, no run
  -> transactionally insert unique claim + started run + advanced cursor + fact
       lost claim -> discard process-local candidate, no Session
       won claim  -> ordinary Session create -> terminal run persistence
```

Rate-limit and retryable validation failures produce a failed run and keep the
Deployment active. Unrecoverable Environment, Resource, Vault, subagent, or
network-policy failures produce a failed run and auto-pause the Deployment with
the same typed reason. Archiving the primary Agent cascades immediately; a stale
or deleted primary detected at a tick archives without a DeploymentRun.

### Recovery and webhooks

At startup all Deployment and DeploymentRun rows are decoded, identity/owner
checked, and sequence counters recovered. The persisted cron cursor prevents a
restart from reseeding the schedule. Concurrent replicas may both calculate an
occurrence, but only the unique claim transaction can expose a durable run.

Deployment and DeploymentRun mutations write `ManagedLifecycleFact` in the same
transaction as the business row. The one webhook sink retries the stable fact id
until all matching subscriptions accept it. Supported emitted events are
`deployment.created`, `deployment.updated`, `deployment.paused`,
`deployment.unpaused`, `deployment.archived`, `deployment_run.started`,
`deployment_run.succeeded`, and `deployment_run.failed`.

## Official compatibility and Awaken extension boundary

The HTTP resources and behaviors follow the current Managed Agents SDK:
`create`, `retrieve`, `update`, `list`, `archive`, `pause`, `unpause`, `run`,
plus DeploymentRun `retrieve` and `list`. There is no public Deployment delete
method; archive is the API terminal operation. Schedule previews retain exact
cron instants, while execution starts at or after the bounded jitter.

Automatic Dream policy is not part of Anthropic's Deployment or Dreams API. It
is an Awaken extension at `/v1/dream_policies/{memory_store_id}`, is default-off,
requires both Managed and Dreaming beta capabilities, persists its own interval
cursor, and submits only through the canonical Dream create path.

## Failure and consistency invariants

- A Workspace cannot retrieve, mutate, run, or list another Workspace's rows.
- A persisted Deployment always contains one concrete Agent version.
- A scheduled occurrence has at most one durable claim and one DeploymentRun.
- Deployment initial Events commit through Session create, never a follow-up path.
- DeploymentRun records only Session creation outcome, not Session lifecycle.
- Business mutation and lifecycle fact share one repository transaction.
- One periodic driver and one lifecycle outbox serve all Managed resource facts.

## Test design and coverage

Cause/effect and decision rules live in comments beside their tests.

| Rule family | Owning test |
|---|---|
| durable restore, Workspace isolation, two-replica claim, lifecycle facts | `durable_repository_restores_scope_and_claims_each_occurrence_once` |
| Agent latest/pinned resolution and invalid lifecycle | `deployment_resolves_and_freezes_the_authoritative_agent_version` |
| missing/archived primary Agent | `missing_or_archived_primary_agent_archives_without_a_run` |
| schedule syntax/timezone and bounded stable jitter | `validate_schedule_rejects_a_malformed_cron`, `execution_jitter_is_stable_and_obeys_all_interval_bounds` |
| capacity, pause/unpause, terminal archive, manual while paused, failure auto-pause | Deployment route decision-table unit tests |
| official cross-module HTTP -> ordinary Session/Event behavior | `managed_deployment_e2e` |
| official TypeScript SDK CRUD, manual run, filters, pause/unpause and archive | `management_deployments_e2e.mjs` |
| official TypeScript SDK cron scheduling and persisted cursor behavior | `management_deployment_schedule_e2e.mjs` |
| durable lifecycle retry and webhook projection | `awaken-webhook-managed` CRUD/outbox tests |

## References

- [Anthropic Managed Agents overview](https://platform.claude.com/docs/en/managed-agents/overview)
- [Anthropic Scheduled deployments](https://platform.claude.com/docs/en/managed-agents/scheduled-deployments)
- [Anthropic TypeScript SDK Deployments](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/beta/deployments.ts)
- [Anthropic TypeScript SDK DeploymentRuns](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/beta/deployment-runs.ts)
- [Managed Dream](managed-dream.md)
- [ADR-0014: Scheduled Delivery](../adr/0014-scheduled-delivery.md)
